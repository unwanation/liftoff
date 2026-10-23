//! `DomainService` (test cases DOM-01..DOM-07).
//!
//!
//!
//! - **managed** `<label>.<base>`: the label is a [`Label`] not on
//!   `[domains] reserved`; no DNS step (the base is a wildcard); the row,
//!   then the proxy (503 page until bound), then the `domain_created` hook
//!   (DOM-01, DOM-02);
//! - **external**: verified through the [`Resolver`](crate::ports::Resolver)
//!   by a CNAME to the base or an A record to `public_ip`, then
//!   `verified_at`, the row, the proxy and the hook (DOM-03..DOM-05);
//!   re-verified daily, a lost record is a warning and the route stays
//!   (DOM-07);
//! - `remove` only without a binding: the row, the proxy, the
//!   `domain_removed` hook (DOM-06);
//! - `set_proxied` only for managed domains with a DNS module.

use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::sync::Arc;

use super::authorizer::Authorizer;
use super::settings::Settings;
use super::support::{apply_proxy, authorize, authorized_domain, authorized_project};
use crate::domain::{
    Domain, DomainId, DomainKind, Host, Label, ModuleName, Project, ProjectName, Timestamp, User,
};
use crate::error::{Error, System};
use crate::ports::{
    Action, Clock as _, HookResult, Hooks as _, ModuleRequest, Ports, Resolver as _, Resource,
    Rng as _, Store as _,
};

/// The module that keeps the DNS records of managed domains.
const DNS_MODULE: &str = "dns.cloudflare";

/// What a fresh `<random label>.<base>` resolves to: the wildcard DNS
/// check of `lof doctor` (`GET /server`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WildcardDns {
    /// `[domains] base`.
    pub base: Host,
    /// `[domains] public_ip`.
    pub public_ip: Ipv4Addr,
    /// The name that was asked: a label no one has created.
    pub probe: Host,
    /// What it resolved to.
    pub verdict: WildcardVerdict,
}

/// The verdict of [`WildcardDns`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WildcardVerdict {
    /// A CNAME to the base or an A record to `public_ip`, as an external
    /// domain is verified (DOM-03, DOM-04).
    PointsAtUs,
    /// A records, none of them `public_ip` (a proxied wildcard, a stale
    /// record).
    Elsewhere(Vec<Ipv4Addr>),
    /// No record: managed domains do not resolve.
    Missing,
}

/// A domain with the name of its project, as `GET /domains` shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DomainRecord {
    /// The project.
    pub project: ProjectName,
    /// The domain.
    pub domain: Domain,
}

/// See the [module documentation](self).
#[derive(Debug)]
pub struct DomainService<P> {
    ports: Arc<P>,
    settings: Arc<Settings>,
}

impl<P> Clone for DomainService<P> {
    fn clone(&self) -> Self {
        Self {
            ports: Arc::clone(&self.ports),
            settings: Arc::clone(&self.settings),
        }
    }
}

impl<P: Ports> DomainService<P> {
    /// A service over `ports` configured by `settings`.
    #[must_use]
    pub const fn new(ports: Arc<P>, settings: Arc<Settings>) -> Self {
        Self { ports, settings }
    }

    /// The domains `actor` may see with their project's name, by host:
    /// every one for the admin, those `authorize` allows one by one for
    /// anyone else (TEN-02).
    ///
    /// # Errors
    ///
    /// Whatever the store reports; [`Error::Internal`] for a domain whose
    /// project is not stored.
    pub async fn list(&self, actor: &User) -> Result<Vec<DomainRecord>, Error> {
        authorize(
            self.ports.as_ref(),
            actor,
            Action::DomainList,
            &Resource::Admin,
        )
        .await?;
        let store = self.ports.store();
        let names: BTreeMap<_, _> = store
            .projects()
            .await?
            .into_iter()
            .map(|project| (project.id, project.name))
            .collect();
        let domains = Authorizer::new(self.ports.as_ref())
            .visible(
                actor,
                Action::DomainList,
                store.domains().await?,
                |domain| Resource::Domain(domain.id),
            )
            .await;
        domains
            .into_iter()
            .map(|domain| {
                let project = names.get(&domain.project_id).cloned().ok_or_else(|| {
                    Error::internal_msg(format!("domain {} has no project", domain.host))
                })?;
                Ok(DomainRecord { project, domain })
            })
            .collect()
    }

    /// Whether Caddy may get a certificate for `host` on demand (`GET /internal/tls-ask`): only for a known external domain
    /// whose DNS was verified. Managed hosts are covered by the base
    /// wildcard and never asked about.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] naming the host for an unknown, managed or
    /// unverified host; whatever the store reports.
    pub async fn allow_certificate(&self, host: &Host) -> Result<(), Error> {
        match self.ports.store().domain_by_host(host).await? {
            Some(domain) if domain.kind == DomainKind::External && domain.verified_at.is_some() => {
                Ok(())
            }
            _ => Err(Error::not_found(format!("verified domain {host}"))),
        }
    }

    /// Adds the managed domain `<label>.<base>` to `project` (DOM-01);
    /// `proxied` (`lof domain add --proxied`) goes into the row before
    /// `domain_created`, so `dns.cloudflare` creates the record proxied
    /// (DNS-01).
    ///
    /// # Errors
    ///
    /// - [`Error::Forbidden`] unless `actor` owns the project (SEC-10);
    /// - [`Error::ModuleRequired`] (`dns.cloudflare`) for `proxied` when
    ///   the module is not installed or disabled, before anything is
    ///   stored;
    /// - [`Error::Validation`] on field `label` for a reserved label;
    /// - [`Error::NotFound`] for an unknown project;
    /// - [`Error::Conflict`] when the host is taken (DOM-02);
    /// - [`Error::Upstream`] when the proxy rejects the configuration; the
    ///   row is taken back.
    pub async fn add_managed(
        &self,
        actor: &User,
        project: &ProjectName,
        label: Label,
        proxied: bool,
    ) -> Result<Domain, Error> {
        let project = authorized_project(
            self.ports.as_ref(),
            actor,
            Action::DomainAddManaged,
            project,
        )
        .await?;
        if self.settings.reserved.contains(&label) {
            return Err(reserved(&label));
        }
        if proxied {
            let module = ModuleName::new(DNS_MODULE)?;
            match self.ports.hooks().module_available(&module).await {
                Ok(()) => {}
                Err(Error::NotFound(_)) => return Err(Error::module_required(DNS_MODULE)),
                Err(error) => return Err(error),
            }
        }
        let host = self.admit_managed(&label).await?;
        let mut domain = self.new_domain(&project, host, DomainKind::Managed, None);
        domain.proxied = proxied;
        self.store_and_announce(domain).await
    }

    /// Adds the external domain `host` to `project` once its DNS points at
    /// liftoff: a CNAME to the base (DOM-03) or an A record to the public
    /// IP (DOM-04).
    ///
    /// # Errors
    ///
    /// - [`Error::Forbidden`] unless `actor` owns the project (SEC-10);
    /// - [`Error::Validation`] naming the expected record when neither is
    ///   there (DOM-05);
    /// - [`Error::NotFound`] for an unknown project;
    /// - [`Error::Conflict`] when the host is taken;
    /// - [`Error::Upstream`] when the resolver or the proxy fail.
    pub async fn add_external(
        &self,
        actor: &User,
        project: &ProjectName,
        host: Host,
    ) -> Result<Domain, Error> {
        let project = authorized_project(
            self.ports.as_ref(),
            actor,
            Action::DomainAddExternal,
            project,
        )
        .await?;
        self.admit_external(&host).await?;
        let base = &self.settings.base;
        if !self.points_at_us(&host).await? {
            return Err(Error::invalid(
                "host",
                format!(
                    "{host} does not point at liftoff: expected a CNAME record to {base} \
                     or an A record to {}",
                    self.settings.public_ip
                ),
            ));
        }
        let verified_at = Some(self.ports.clock().now());
        let domain = self.new_domain(&project, host, DomainKind::External, verified_at);
        self.store_and_announce(domain).await
    }

    /// Removes an unbound domain: the row, the proxy, the `domain_removed`
    /// hook (DOM-06).
    ///
    /// # Errors
    ///
    /// - [`Error::Forbidden`] unless `actor` owns the domain (SEC-10);
    /// - [`Error::NotFound`] for an unknown host;
    /// - [`Error::Conflict`] while the domain is bound (DOM-06);
    /// - [`Error::Upstream`] when the proxy rejects the configuration; the
    ///   row is put back.
    pub async fn remove(&self, actor: &User, host: &Host) -> Result<(), Error> {
        let store = self.ports.store();
        let domain =
            authorized_domain(self.ports.as_ref(), actor, Action::DomainRemove, host).await?;
        if store.binding_by_domain(domain.id).await?.is_some() {
            return Err(Error::conflict(format!("{host} is bound; unbind it first")));
        }
        store.delete_domain(domain.id).await?;
        if let Err(error) = apply_proxy(self.ports.as_ref(), &self.settings).await {
            // The proxy kept its old configuration, which still has the
            // site: put the row back so the store agrees.
            if let Err(restore) = store.insert_domain(&domain).await {
                tracing::error!(error = %restore, %host, "could not put back a domain after a refused proxy configuration");
            }
            return Err(error);
        }
        if let HookResult::Deny(reason) = self.ports.hooks().domain_removed(&domain).await {
            // The domain is gone already; a module objecting now can only
            // be reported.
            tracing::warn!(%host, %reason, "domain_removed hook denied after the domain was removed");
        }
        Ok(())
    }

    /// Sets whether a DNS module proxies a managed domain:
    /// the flag goes to the record through the route `PATCH
    /// /records/<host>` `{"proxied"}` of the module `dns.cloudflare`, then into the domain's row. The route is
    /// not in the module's manifest, so `/api/v1/m/` does not reach it:
    /// only this operation, after `authorize`, does.
    ///
    /// # Errors
    ///
    /// - [`Error::Forbidden`] unless `actor` owns the domain (SEC-10);
    /// - [`Error::ModuleRequired`] (`dns.cloudflare`) when the module is
    ///   not installed or disabled;
    /// - [`Error::Validation`] for an external domain, or one the module
    ///   keeps no record of (added before it was installed);
    /// - [`Error::NotFound`] for an unknown host;
    /// - [`Error::Upstream`] when the module or Cloudflare fail, with the
    ///   `detail` of the module's problem (its whole answer goes to the
    ///   log only); whatever the store reports, after the record's flag
    ///   was put back.
    pub async fn set_proxied(
        &self,
        actor: &User,
        host: &Host,
        proxied: bool,
    ) -> Result<DomainRecord, Error> {
        let mut domain =
            authorized_domain(self.ports.as_ref(), actor, Action::DomainSetProxied, host).await?;
        if domain.kind == DomainKind::External {
            return Err(Error::invalid(
                "host",
                format!("{host} is external: only managed domains can be proxied"),
            ));
        }
        let previous = domain.proxied;
        self.proxy_record(actor, host, proxied).await?;
        domain.proxied = proxied;
        if let Err(error) = self.ports.store().update_domain(&domain).await {
            // The record changed but the row did not: put the record's
            // flag back so both agree.
            if let Err(undo) = self.proxy_record(actor, host, previous).await {
                tracing::error!(error = %undo, %host, "could not put back the proxied flag of a DNS record after the domain was not stored");
            }
            return Err(error);
        }
        let project = self
            .ports
            .store()
            .project(domain.project_id)
            .await?
            .ok_or_else(|| Error::internal_msg(format!("domain {host} has no project")))?;
        Ok(DomainRecord {
            project: project.name,
            domain,
        })
    }

    /// Re-verifies the DNS of every external domain (the daily job): a
    /// lost record sets `record_lost_at` (a warning in the
    /// domain's status) and keeps the route; a record that is back clears
    /// it and renews `verified_at` (DOM-07). Returns the external domains
    /// as stored afterwards, by host.
    ///
    /// # Errors
    ///
    /// Whatever the store reports. A failing resolver leaves the domain
    /// as it was.
    pub async fn reverify_external(&self) -> Result<Vec<Domain>, Error> {
        let store = self.ports.store();
        let mut checked = Vec::new();
        for mut domain in store.domains().await? {
            if domain.kind != DomainKind::External {
                continue;
            }
            match self.points_at_us(&domain.host).await {
                Ok(true) => {
                    domain.verified_at = Some(self.ports.clock().now());
                    domain.record_lost_at = None;
                    store.update_domain(&domain).await?;
                }
                Ok(false) => {
                    if domain.record_lost_at.is_none() {
                        domain.record_lost_at = Some(self.ports.clock().now());
                        store.update_domain(&domain).await?;
                    }
                    tracing::warn!(host = %domain.host, "external domain no longer points at liftoff");
                }
                Err(error) => {
                    tracing::warn!(host = %domain.host, %error, "could not re-verify an external domain");
                }
            }
            checked.push(domain);
        }
        Ok(checked)
    }

    /// Hands `proxied` to the route `PATCH /records/<host>` of
    /// [`DNS_MODULE`] as `actor` (see [`DomainService::set_proxied`]).
    async fn proxy_record(&self, actor: &User, host: &Host, proxied: bool) -> Result<(), Error> {
        let module = ModuleName::new(DNS_MODULE)?;
        let request = ModuleRequest {
            method: "PATCH".to_owned(),
            path: format!("/records/{host}"),
            headers: vec![("content-type".to_owned(), "application/json".to_owned())],
            body: format!("{{\"proxied\":{proxied}}}").into_bytes(),
        };
        let answered = match self
            .ports
            .hooks()
            .handle_route(&module, Some(actor), request)
            .await
        {
            Ok(answered) => answered,
            Err(Error::NotFound(_)) => return Err(Error::module_required(DNS_MODULE)),
            Err(error) => return Err(error),
        };
        match answered.status {
            200..=299 => Ok(()),
            404 => Err(Error::invalid(
                "host",
                format!("{host} has no DNS record of {DNS_MODULE}"),
            )),
            status => {
                let body = String::from_utf8_lossy(&answered.body);
                tracing::warn!(%host, status, %body, "the DNS module failed to set the proxied flag");
                let detail = serde_json::from_slice::<serde_json::Value>(&answered.body)
                    .ok()
                    .and_then(|problem| problem.get("detail")?.as_str().map(str::to_owned));
                Err(Error::upstream(
                    System::Dns,
                    detail.map_or_else(
                        || format!("{DNS_MODULE} answered {status}"),
                        |detail| format!("{DNS_MODULE} answered {status}: {detail}"),
                    ),
                ))
            }
        }
    }

    /// [`Error::Conflict`] when any project holds `host` (DOM-02).
    /// The checks of [`DomainService::add_managed`] that need neither the
    /// project nor the outside world: `<label>.<base>`, once the label is
    /// not reserved and the host is free. A `lof create` plan runs them
    /// when it is built (CRE-07).
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] on `label` for a reserved label;
    /// [`Error::Conflict`] when the host is taken; whatever the store
    /// reports.
    pub(super) async fn admit_managed(&self, label: &Label) -> Result<Host, Error> {
        if self.settings.reserved.contains(label) {
            return Err(reserved(label));
        }
        let host = Host::under(label, &self.settings.base)?;
        self.ensure_host_free(&host).await?;
        Ok(host)
    }

    /// The checks of [`DomainService::add_external`] that need neither the
    /// project nor DNS: `host` is not under the base and is free (CRE-07).
    ///
    /// # Errors
    ///
    /// [`Error::Validation`] on `host` for a host under the base;
    /// [`Error::Conflict`] when it is taken; whatever the store reports.
    pub(super) async fn admit_external(&self, host: &Host) -> Result<(), Error> {
        let base = &self.settings.base;
        if host == base || host.label_in(base).is_some() {
            return Err(Error::invalid(
                "host",
                format!("{host} is under {base}: add it as a managed domain"),
            ));
        }
        self.ensure_host_free(host).await
    }

    async fn ensure_host_free(&self, host: &Host) -> Result<(), Error> {
        if self.ports.store().domain_by_host(host).await?.is_some() {
            return Err(host_taken(host));
        }
        Ok(())
    }

    /// Resolves a label no one has created under the base (a fresh ULID
    /// each time, so no cached or explicit record answers) and says
    /// whether the wildcard points at this server.
    ///
    /// # Errors
    ///
    /// Whatever the resolver reports.
    pub async fn wildcard_dns(&self) -> Result<WildcardDns, Error> {
        let ulid = self.ports.rng().ulid().to_string().to_ascii_lowercase();
        let label = Label::new(&format!("lof-{ulid}"))?;
        let probe = Host::under(&label, &self.settings.base)?;
        let verdict = if self.points_at_us(&probe).await? {
            WildcardVerdict::PointsAtUs
        } else {
            match self.ports.resolver().a(&probe).await? {
                addresses if addresses.is_empty() => WildcardVerdict::Missing,
                addresses => WildcardVerdict::Elsewhere(addresses),
            }
        };
        Ok(WildcardDns {
            base: self.settings.base.clone(),
            public_ip: self.settings.public_ip,
            probe,
            verdict,
        })
    }

    /// Whether `host` has a CNAME to the base or an A record to the public
    /// IP (DOM-03, DOM-04).
    async fn points_at_us(&self, host: &Host) -> Result<bool, Error> {
        let resolver = self.ports.resolver();
        if resolver
            .cname(host)
            .await?
            .iter()
            .any(|target| *target == self.settings.base)
        {
            return Ok(true);
        }
        Ok(resolver.a(host).await?.contains(&self.settings.public_ip))
    }

    pub(super) fn new_domain(
        &self,
        project: &Project,
        host: Host,
        kind: DomainKind,
        verified_at: Option<Timestamp>,
    ) -> Domain {
        Domain {
            id: DomainId::new(self.ports.rng().ulid()),
            tenant_id: project.tenant_id,
            project_id: project.id,
            host,
            kind,
            verified_at,
            record_lost_at: None,
            proxied: false,
            created_at: self.ports.clock().now(),
        }
    }

    /// Stores `domain`, pushes the proxy configuration (503 page until
    /// bound) and calls the `domain_created` hook. A refused configuration
    /// or a hook that denies takes the row back; after a deny,
    /// `domain_removed` lets the modules that already continued (a DNS
    /// record of `dns.cloudflare`) undo their part.
    pub(super) async fn store_and_announce(&self, domain: Domain) -> Result<Domain, Error> {
        let store = self.ports.store();
        store.insert_domain(&domain).await?;
        if let Err(error) = apply_proxy(self.ports.as_ref(), &self.settings).await {
            let _gone = self.take_back(&domain, false).await;
            return Err(error);
        }
        if let HookResult::Deny(reason) = self.ports.hooks().domain_created(&domain).await {
            if self.take_back(&domain, true).await
                && let HookResult::Deny(late) = self.ports.hooks().domain_removed(&domain).await
            {
                tracing::warn!(host = %domain.host, reason = %late, "domain_removed hook denied after a denied domain was taken back");
            }
            return Err(Error::forbidden(reason));
        }
        Ok(domain)
    }

    /// Deletes a just stored domain again; `reapply` when the proxy already
    /// took a configuration with it. Whether the row is gone.
    async fn take_back(&self, domain: &Domain, reapply: bool) -> bool {
        if let Err(error) = self.ports.store().delete_domain(domain.id).await {
            tracing::error!(%error, host = %domain.host, "could not take back a domain");
            return false;
        }
        if reapply && let Err(error) = apply_proxy(self.ports.as_ref(), &self.settings).await {
            tracing::error!(%error, host = %domain.host, "could not drop the site of a taken back domain");
        }
        true
    }
}

/// What the conflict of a taken host says after the host (CRE-07).
const TAKEN: &str = " is taken";

/// The conflict of `host` being taken (DOM-02, CRE-07): `<host> is taken`,
/// which [`taken_host`] recognises.
fn host_taken(host: &Host) -> Error {
    Error::conflict(format!("{host}{TAKEN}"))
}

/// The host a conflict names as taken, from the conflict's message (the
/// `detail` of the server's `409`): an adapter offers another domain
/// for it (CRE-07). `None` for any other conflict.
#[must_use]
pub fn taken_host(message: &str) -> Option<Host> {
    let host = message.strip_suffix(TAKEN)?;
    Host::new(host).ok()
}

/// The refusal of a reserved label.
fn reserved(label: &Label) -> Error {
    Error::invalid("label", format!("{label} is reserved"))
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use time::OffsetDateTime;

    use super::*;
    use crate::domain::{DomainKind, Timestamp};
    use crate::error::System;
    use crate::fakes::{Call, HookCall, ResolveCall, World};
    use crate::ports::ModuleResponse;
    use crate::proxy_config::{ProxyConfig, Route};

    /// 2026-10-13T00:00:00Z, the fake clock's start.
    const DAY_ONE: i64 = 1_791_849_600;
    /// 2026-10-14T00:00:00Z.
    const DAY_TWO: i64 = 1_791_936_000;

    fn at(unix: i64) -> Timestamp {
        OffsetDateTime::from_unix_timestamp(unix).unwrap()
    }

    fn project(raw: &str) -> ProjectName {
        ProjectName::new(raw).unwrap()
    }

    fn label(raw: &str) -> Label {
        Label::new(raw).unwrap()
    }

    fn host(raw: &str) -> Host {
        Host::new(raw).unwrap()
    }

    fn route_of(config: Option<ProxyConfig>, raw: &str) -> Option<Route> {
        let wanted = host(raw);
        config?
            .sites
            .into_iter()
            .find(|site| site.host == wanted)
            .map(|site| site.route)
    }

    /// Proxy applies and domain hooks, in order, as short strings.
    fn proxy_and_hooks(world: &World) -> Vec<String> {
        world
            .log()
            .calls()
            .into_iter()
            .filter_map(|call| match call {
                Call::Proxy(_) => Some("apply".to_owned()),
                Call::Hook(HookCall::DomainCreated { host, .. }) => {
                    Some(format!("domain_created {host}"))
                }
                Call::Hook(HookCall::DomainRemoved { host, .. }) => {
                    Some(format!("domain_removed {host}"))
                }
                _ => None,
            })
            .collect()
    }

    fn resolved(world: &World) -> bool {
        world
            .log()
            .calls()
            .iter()
            .any(|call| matches!(call, Call::Resolve(_)))
    }

    /// Stores the external domain `www.shop.example` of `shop`, verified on
    /// day one.
    async fn verified_external(world: &World) -> Domain {
        let mut domain = world
            .records
            .domain(&world.shop, "www.shop.example")
            .unwrap();
        domain.kind = DomainKind::External;
        domain.verified_at = Some(at(DAY_ONE));
        world.ports.store.insert_domain(&domain).await.unwrap();
        domain
    }

    mod add_managed {
        use super::*;

        // TC: REST-06, SEC-10 (the owner check: single-user mode, the admin)
        #[tokio::test]
        async fn forbidden_before_any_change_when_actor_is_not_admin() {
            let world = World::new().await;
            let carol = world
                .add_user("carol", crate::domain::UserRole::User)
                .await
                .unwrap();
            world.log().clear();
            let error = world
                .domains()
                .add_managed(&carol, &project("shop"), label("blog"), false)
                .await
                .unwrap_err();
            assert!(
                matches!(error, Error::Forbidden(_)) && world.log().only_authorized(),
                "refused before any change, got {error:?} {:?}",
                world.log().calls()
            );
        }

        // TC: DOM-01
        #[tokio::test]
        async fn stores_label_under_base_when_label_free() {
            let world = World::new().await;
            world
                .domains()
                .add_managed(&world.owner, &project("shop"), label("blog"), false)
                .await
                .unwrap();
            let stored = world
                .ports
                .store
                .domain_by_host(&host("blog.apps.test"))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                (stored.kind, stored.project_id, stored.proxied),
                (DomainKind::Managed, world.shop.id, false)
            );
        }

        // TC: DOM-01
        #[tokio::test]
        async fn returns_host_label_dot_base_when_added() {
            let world = World::new().await;
            let domain = world
                .domains()
                .add_managed(&world.owner, &project("shop"), label("blog"), false)
                .await
                .unwrap();
            assert_eq!(domain.host.as_str(), "blog.apps.test");
        }

        // TC: DOM-01
        #[tokio::test]
        async fn validation_on_label_when_reserved() {
            let world = World::new().await;
            let err = world
                .domains()
                .add_managed(&world.owner, &project("shop"), label("liftoff"), false)
                .await
                .unwrap_err();
            assert!(
                matches!(&err, Error::Validation(v) if v.field == "label"),
                "{err:?}"
            );
        }

        // TC: DOM-01 (the regex is the label type's, at the boundary)
        #[test]
        fn validation_on_label_when_not_label_shape() {
            let fields: Vec<&str> = ["-a", "Blog", "a.b", ""]
                .into_iter()
                .filter_map(|raw| Label::new(raw).err().map(|e| e.field))
                .collect();
            assert_eq!(fields, vec!["label", "label", "label", "label"]);
        }

        // TC: DOM-01 (503 page until bound)
        #[tokio::test]
        async fn routes_new_domain_to_503_when_added() {
            let world = World::new().await;
            world
                .domains()
                .add_managed(&world.owner, &project("shop"), label("blog"), false)
                .await
                .unwrap();
            assert_eq!(
                route_of(world.ports.proxy.applied(), "blog.apps.test"),
                Some(Route::Unavailable)
            );
        }

        // TC: DOM-01 (no DNS step: the base is a wildcard)
        #[tokio::test]
        async fn never_resolves_dns_when_added() {
            let world = World::new().await;
            world
                .domains()
                .add_managed(&world.owner, &project("shop"), label("blog"), false)
                .await
                .unwrap();
            assert!(!resolved(&world), "{:?}", world.log().calls());
        }

        // TC: DOM-01
        #[tokio::test]
        async fn calls_domain_created_with_host_after_apply_when_added() {
            let world = World::new().await;
            world
                .domains()
                .add_managed(&world.owner, &project("shop"), label("blog"), false)
                .await
                .unwrap();
            assert_eq!(
                proxy_and_hooks(&world),
                vec![
                    "apply".to_owned(),
                    "domain_created blog.apps.test".to_owned()
                ]
            );
        }

        // TC: DOM-01 (compensation: a refused config leaves no row)
        #[tokio::test]
        async fn takes_row_back_without_hook_when_proxy_rejects() {
            let world = World::new().await;
            world
                .ports
                .proxy
                .fail_next("apply", Error::upstream(System::Caddy, "bad config"));
            let result = world
                .domains()
                .add_managed(&world.owner, &project("shop"), label("blog"), false)
                .await;
            let stored = world
                .ports
                .store
                .domain_by_host(&host("blog.apps.test"))
                .await
                .unwrap();
            let hooked = world
                .ports
                .hooks
                .calls()
                .iter()
                .any(|call| matches!(call, HookCall::DomainCreated { .. }));
            assert!(
                matches!(
                    &result,
                    Err(Error::Upstream { system: System::Caddy, detail }) if detail == "bad config"
                ) && stored.is_none()
                    && !hooked,
                "{result:?}, stored {stored:?}, hooked {hooked}"
            );
        }

        // TC: DOM-01 (compensation: a denying hook takes the row and the site back)
        #[tokio::test]
        async fn forbidden_and_row_taken_back_when_domain_created_denies() {
            let world = World::new().await;
            world.ports.hooks.answer_next(
                "domain_created",
                HookResult::Deny("no blogs here".to_owned()),
            );
            let result = world
                .domains()
                .add_managed(&world.owner, &project("shop"), label("blog"), false)
                .await;
            let stored = world
                .ports
                .store
                .domain_by_host(&host("blog.apps.test"))
                .await
                .unwrap();
            assert!(
                matches!(&result, Err(Error::Forbidden(why)) if why == "no blogs here")
                    && stored.is_none(),
                "{result:?}, stored {stored:?}"
            );
        }

        // TC: DOM-01 (the proxy drops the site of the denied domain again,
        // then `domain_removed` lets the modules that already continued,
        // `dns.cloudflare` with its record, undo their part)
        #[tokio::test]
        async fn drops_site_again_when_domain_created_denies() {
            let world = World::new().await;
            world
                .ports
                .hooks
                .answer_next("domain_created", HookResult::Deny("no".to_owned()));
            let _denied = world
                .domains()
                .add_managed(&world.owner, &project("shop"), label("blog"), false)
                .await;
            assert_eq!(
                (
                    proxy_and_hooks(&world),
                    route_of(world.ports.proxy.applied(), "blog.apps.test")
                ),
                (
                    vec![
                        "apply".to_owned(),
                        "domain_created blog.apps.test".to_owned(),
                        "apply".to_owned(),
                        "domain_removed blog.apps.test".to_owned(),
                    ],
                    None
                )
            );
        }

        // TC: DOM-02
        #[tokio::test]
        async fn conflicts_when_host_taken() {
            let world = World::new().await;
            let err = world
                .domains()
                .add_managed(&world.owner, &project("shop"), label("shop"), false)
                .await
                .unwrap_err();
            assert!(matches!(err, Error::Conflict(_)), "{err:?}");
        }

        // TC: DOM-02, CRE-07 (adapters recognise the conflict of a taken
        // host by `taken_host`, whose message the server's `409` carries)
        #[tokio::test]
        async fn names_taken_host_when_host_taken() {
            let world = World::new().await;
            let err = world
                .domains()
                .add_managed(&world.owner, &project("shop"), label("shop"), false)
                .await
                .unwrap_err();
            assert_eq!(taken_host(&err.to_string()), Some(host("shop.apps.test")));
        }

        // TC: CRE-07 (a project's conflict names no taken host)
        #[test]
        fn names_no_host_when_conflict_is_not_a_hosts() {
            assert_eq!(taken_host("project demo already exists"), None);
        }

        // TC: DOM-02 (hosts are global, not per project)
        #[tokio::test]
        async fn conflicts_when_host_taken_by_other_project() {
            let world = World::new().await;
            let blog = world.add_project("blog", "owner/blog").await.unwrap();
            world.add_domain(&blog, "news.apps.test").await.unwrap();
            let err = world
                .domains()
                .add_managed(&world.owner, &project("shop"), label("news"), false)
                .await
                .unwrap_err();
            assert!(matches!(err, Error::Conflict(_)), "{err:?}");
        }

        // TC: DOM-02
        #[tokio::test]
        async fn applies_nothing_when_host_taken() {
            let world = World::new().await;
            let _refused = world
                .domains()
                .add_managed(&world.owner, &project("shop"), label("shop"), false)
                .await;
            assert_eq!(proxy_and_hooks(&world), Vec::<String>::new());
        }
    }

    mod add_managed_proxied {
        use super::*;

        // TC: DNS-01 (`--proxied` needs the DNS module; nothing is stored without it)
        #[tokio::test]
        async fn module_required_before_any_change_when_dns_module_missing() {
            let world = World::new().await;
            world.log().clear();
            let refused = world
                .domains()
                .add_managed(&world.owner, &project("shop"), label("blog"), true)
                .await;
            let stored = world
                .ports
                .store
                .domain_by_host(&host("blog.apps.test"))
                .await
                .unwrap();
            assert!(
                matches!(&refused, Err(Error::ModuleRequired(module)) if module == "dns.cloudflare")
                    && stored.is_none()
                    && proxy_and_hooks(&world).is_empty(),
                "{refused:?}, stored {stored:?}, {:?}",
                proxy_and_hooks(&world)
            );
        }

        // TC: DNS-01 (the flag goes into the row the hook payload carries)
        #[tokio::test]
        async fn stores_proxied_domain_when_dns_module_available() {
            let world = World::new().await;
            world.ports.hooks.make_available("dns.cloudflare");
            let added = world
                .domains()
                .add_managed(&world.owner, &project("shop"), label("blog"), true)
                .await
                .unwrap();
            let stored = world
                .ports
                .store
                .domain_by_host(&host("blog.apps.test"))
                .await
                .unwrap()
                .map(|domain| domain.proxied);
            assert_eq!((added.proxied, stored), (true, Some(true)));
        }
    }

    mod add_external {
        use super::*;

        // TC: REST-06, SEC-10 (the owner check: single-user mode, the admin)
        #[tokio::test]
        async fn forbidden_before_any_change_when_actor_is_not_admin() {
            let world = World::new().await;
            let carol = world
                .add_user("carol", crate::domain::UserRole::User)
                .await
                .unwrap();
            world.log().clear();
            let error = world
                .domains()
                .add_external(
                    &carol,
                    &project("shop"),
                    Host::new("shop.example.org").unwrap(),
                )
                .await
                .unwrap_err();
            assert!(
                matches!(error, Error::Forbidden(_)) && world.log().only_authorized(),
                "refused before any change, got {error:?} {:?}",
                world.log().calls()
            );
        }

        // TC: DOM-03 (a host under the base is a managed domain, never resolved)
        #[tokio::test]
        async fn validation_on_host_without_resolving_when_host_under_base() {
            let world = World::new().await;
            let err = world
                .domains()
                .add_external(&world.owner, &project("shop"), host("blog.apps.test"))
                .await
                .unwrap_err();
            assert!(
                matches!(&err, Error::Validation(v) if v.field == "host") && !resolved(&world),
                "{err:?}"
            );
        }

        // TC: DOM-03 (the base itself is not an external domain either)
        #[tokio::test]
        async fn validation_when_host_is_base() {
            let world = World::new().await;
            let err = world
                .domains()
                .add_external(&world.owner, &project("shop"), host("apps.test"))
                .await
                .unwrap_err();
            assert!(matches!(err, Error::Validation(_)), "{err:?}");
        }

        // TC: DOM-03
        #[tokio::test]
        async fn sets_verified_at_when_cname_points_at_base() {
            let world = World::new().await;
            world
                .ports
                .resolver
                .set_cname(&host("www.shop.example"), vec![host("apps.test")]);
            world
                .domains()
                .add_external(&world.owner, &project("shop"), host("www.shop.example"))
                .await
                .unwrap();
            let stored = world
                .ports
                .store
                .domain_by_host(&host("www.shop.example"))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                (stored.kind, stored.verified_at, stored.project_id),
                (DomainKind::External, Some(at(DAY_ONE)), world.shop.id)
            );
        }

        // TC: DOM-03 (the route exists: 503 until bound)
        #[tokio::test]
        async fn routes_domain_when_cname_points_at_base() {
            let world = World::new().await;
            world
                .ports
                .resolver
                .set_cname(&host("www.shop.example"), vec![host("apps.test")]);
            world
                .domains()
                .add_external(&world.owner, &project("shop"), host("www.shop.example"))
                .await
                .unwrap();
            assert_eq!(
                route_of(world.ports.proxy.applied(), "www.shop.example"),
                Some(Route::Unavailable)
            );
        }

        // TC: DOM-03 (hook as for managed domains)
        #[tokio::test]
        async fn calls_domain_created_after_apply_when_verified() {
            let world = World::new().await;
            world
                .ports
                .resolver
                .set_cname(&host("www.shop.example"), vec![host("apps.test")]);
            world
                .domains()
                .add_external(&world.owner, &project("shop"), host("www.shop.example"))
                .await
                .unwrap();
            assert_eq!(
                proxy_and_hooks(&world),
                vec![
                    "apply".to_owned(),
                    "domain_created www.shop.example".to_owned()
                ]
            );
        }

        // TC: DOM-04
        #[tokio::test]
        async fn sets_verified_at_when_a_record_is_public_ip() {
            let world = World::new().await;
            world
                .ports
                .resolver
                .set_a(&host("shop.example"), vec![Ipv4Addr::new(203, 0, 113, 10)]);
            let domain = world
                .domains()
                .add_external(&world.owner, &project("shop"), host("shop.example"))
                .await
                .unwrap();
            assert_eq!(
                (domain.kind, domain.verified_at),
                (DomainKind::External, Some(at(DAY_ONE)))
            );
        }

        // TC: DOM-04 (an A record elsewhere does not count)
        #[tokio::test]
        async fn validation_when_a_record_is_other_ip() {
            let world = World::new().await;
            world
                .ports
                .resolver
                .set_a(&host("shop.example"), vec![Ipv4Addr::new(198, 51, 100, 1)]);
            let err = world
                .domains()
                .add_external(&world.owner, &project("shop"), host("shop.example"))
                .await
                .unwrap_err();
            assert!(matches!(err, Error::Validation(_)), "{err:?}");
        }

        // TC: DOM-05
        #[tokio::test]
        async fn validation_naming_expected_records_when_no_record() {
            let world = World::new().await;
            let err = world
                .domains()
                .add_external(&world.owner, &project("shop"), host("www.shop.example"))
                .await
                .unwrap_err();
            let text = err.to_string();
            assert!(
                matches!(err, Error::Validation(_))
                    && text.contains("CNAME")
                    && text.contains("apps.test")
                    && text.contains("203.0.113.10"),
                "{text}"
            );
        }

        // TC: DOM-05 (a CNAME elsewhere does not count)
        #[tokio::test]
        async fn validation_when_cname_points_elsewhere() {
            let world = World::new().await;
            world
                .ports
                .resolver
                .set_cname(&host("www.shop.example"), vec![host("pages.example.net")]);
            let err = world
                .domains()
                .add_external(&world.owner, &project("shop"), host("www.shop.example"))
                .await
                .unwrap_err();
            assert!(matches!(err, Error::Validation(_)), "{err:?}");
        }

        // TC: DOM-05 (nothing stored, nothing applied)
        #[tokio::test]
        async fn stores_and_applies_nothing_when_no_record() {
            let world = World::new().await;
            let _refused = world
                .domains()
                .add_external(&world.owner, &project("shop"), host("www.shop.example"))
                .await;
            let stored = world
                .ports
                .store
                .domain_by_host(&host("www.shop.example"))
                .await
                .unwrap();
            assert_eq!(
                (stored, proxy_and_hooks(&world)),
                (None, Vec::<String>::new())
            );
        }
    }

    mod remove {
        use super::*;

        // TC: REST-06, SEC-10 (the owner check: single-user mode, the admin)
        #[tokio::test]
        async fn forbidden_before_any_change_when_actor_is_not_admin() {
            let world = World::new().await;
            let carol = world
                .add_user("carol", crate::domain::UserRole::User)
                .await
                .unwrap();
            world.log().clear();
            let error = world
                .domains()
                .remove(&carol, &Host::new("shop.apps.test").unwrap())
                .await
                .unwrap_err();
            assert!(
                matches!(error, Error::Forbidden(_)) && world.log().only_authorized(),
                "refused before any change, got {error:?} {:?}",
                world.log().calls()
            );
        }

        // TC: DOM-06
        #[tokio::test]
        async fn conflicts_when_bound() {
            let world = World::new().await;
            let err = world
                .domains()
                .remove(&world.owner, &host("shop.apps.test"))
                .await
                .unwrap_err();
            assert!(matches!(err, Error::Conflict(_)), "{err:?}");
        }

        // TC: DOM-06
        #[tokio::test]
        async fn keeps_row_without_apply_or_hook_when_bound() {
            let world = World::new().await;
            let _refused = world
                .domains()
                .remove(&world.owner, &host("shop.apps.test"))
                .await;
            let stored = world
                .ports
                .store
                .domain_by_host(&host("shop.apps.test"))
                .await
                .unwrap();
            assert_eq!(
                (stored, proxy_and_hooks(&world)),
                (Some(world.domain.clone()), Vec::<String>::new())
            );
        }

        // TC: DOM-06
        #[tokio::test]
        async fn deletes_row_when_unbound() {
            let world = World::new().await;
            world
                .add_domain(&world.shop, "blog.apps.test")
                .await
                .unwrap();
            world
                .domains()
                .remove(&world.owner, &host("blog.apps.test"))
                .await
                .unwrap();
            let stored = world
                .ports
                .store
                .domain_by_host(&host("blog.apps.test"))
                .await
                .unwrap();
            assert_eq!(stored, None);
        }

        // TC: DOM-06
        #[tokio::test]
        async fn drops_site_then_calls_domain_removed_when_unbound() {
            let world = World::new().await;
            world
                .add_domain(&world.shop, "blog.apps.test")
                .await
                .unwrap();
            world.log().clear();
            world
                .domains()
                .remove(&world.owner, &host("blog.apps.test"))
                .await
                .unwrap();
            assert_eq!(
                (
                    proxy_and_hooks(&world),
                    world.ports.proxy.applied().is_some(),
                    route_of(world.ports.proxy.applied(), "blog.apps.test"),
                ),
                (
                    vec![
                        "apply".to_owned(),
                        "domain_removed blog.apps.test".to_owned()
                    ],
                    true,
                    None
                )
            );
        }

        // TC: DOM-06 (compensation)
        #[tokio::test]
        async fn keeps_row_without_hook_when_proxy_rejects() {
            let world = World::new().await;
            world
                .add_domain(&world.shop, "blog.apps.test")
                .await
                .unwrap();
            world
                .ports
                .proxy
                .fail_next("apply", Error::upstream(System::Caddy, "bad config"));
            let result = world
                .domains()
                .remove(&world.owner, &host("blog.apps.test"))
                .await;
            let stored = world
                .ports
                .store
                .domain_by_host(&host("blog.apps.test"))
                .await
                .unwrap();
            let hooked = world
                .ports
                .hooks
                .calls()
                .iter()
                .any(|call| matches!(call, HookCall::DomainRemoved { .. }));
            assert!(
                result.is_err() && stored.is_some() && !hooked,
                "{result:?}, stored {stored:?}, hooked {hooked}"
            );
        }

        // TC: DOM-06 (the domain is gone already: a late deny is only logged)
        #[tokio::test]
        async fn removes_row_when_domain_removed_denies() {
            let world = World::new().await;
            world
                .add_domain(&world.shop, "blog.apps.test")
                .await
                .unwrap();
            world
                .ports
                .hooks
                .answer_next("domain_removed", HookResult::Deny("keep it".to_owned()));
            let result = world
                .domains()
                .remove(&world.owner, &host("blog.apps.test"))
                .await;
            let stored = world
                .ports
                .store
                .domain_by_host(&host("blog.apps.test"))
                .await
                .unwrap();
            assert!(
                result.is_ok() && stored.is_none(),
                "{result:?}, stored {stored:?}"
            );
        }

        // TC: DOM-06
        #[tokio::test]
        async fn not_found_when_host_unknown() {
            let world = World::new().await;
            let err = world
                .domains()
                .remove(&world.owner, &host("nope.apps.test"))
                .await
                .unwrap_err();
            assert!(matches!(err, Error::NotFound(_)), "{err:?}");
        }
    }

    mod list {
        use super::*;

        // TC: REST-06 (`GET /domains` shows each domain with its project)
        #[tokio::test]
        async fn every_domain_with_its_project_name_by_host_when_listed() {
            let world = World::new().await;
            let blog = world.add_project("blog", "owner/blog").await.unwrap();
            let _blog = world.add_domain(&blog, "blog.apps.test").await.unwrap();
            let listed = world.domains().list(&world.owner).await.unwrap();
            assert_eq!(
                listed
                    .iter()
                    .map(|record| (record.domain.host.to_string(), record.project.to_string()))
                    .collect::<Vec<_>>(),
                vec![
                    ("blog.apps.test".to_owned(), "blog".to_owned()),
                    ("shop.apps.test".to_owned(), "shop".to_owned()),
                ]
            );
        }
    }

    mod set_proxied {
        use super::*;

        // TC: REST-06, SEC-10 (the owner check: single-user mode, the admin)
        #[tokio::test]
        async fn forbidden_before_any_change_when_actor_is_not_admin() {
            let world = World::new().await;
            let carol = world
                .add_user("carol", crate::domain::UserRole::User)
                .await
                .unwrap();
            world.log().clear();
            let error = world
                .domains()
                .set_proxied(&carol, &Host::new("shop.apps.test").unwrap(), true)
                .await
                .unwrap_err();
            assert!(
                matches!(error, Error::Forbidden(_)) && world.log().only_authorized(),
                "refused before any change, got {error:?} {:?}",
                world.log().calls()
            );
        }

        // proxying is the DNS module's; the core alone has none.
        #[tokio::test]
        async fn module_required_when_managed_without_dns_module() {
            let world = World::new().await;
            let err = world
                .domains()
                .set_proxied(&world.owner, &host("shop.apps.test"), true)
                .await
                .unwrap_err();
            assert!(
                matches!(&err, Error::ModuleRequired(module) if module == "dns.cloudflare"),
                "{err:?}"
            );
        }

        // only managed domains can be proxied.
        #[tokio::test]
        async fn validation_when_external() {
            let world = World::new().await;
            verified_external(&world).await;
            let err = world
                .domains()
                .set_proxied(&world.owner, &host("www.shop.example"), true)
                .await
                .unwrap_err();
            assert!(matches!(err, Error::Validation(_)), "{err:?}");
        }

        #[tokio::test]
        async fn not_found_when_host_unknown() {
            let world = World::new().await;
            let err = world
                .domains()
                .set_proxied(&world.owner, &host("nope.apps.test"), true)
                .await
                .unwrap_err();
            assert!(matches!(err, Error::NotFound(_)), "{err:?}");
        }

        fn answered(status: u16, body: &str) -> ModuleResponse {
            ModuleResponse {
                status,
                headers: Vec::new(),
                body: body.as_bytes().to_vec(),
            }
        }

        /// The stored flag of `shop.apps.test`.
        async fn stored_flag(world: &World) -> bool {
            world
                .ports
                .store
                .domain_by_host(&host("shop.apps.test"))
                .await
                .unwrap()
                .unwrap()
                .proxied
        }

        // TC: DNS-05 (the flag goes to the module's route)
        #[tokio::test]
        async fn hands_flag_to_dns_module_route_when_managed() {
            let world = World::new().await;
            world.ports.hooks.answer_route(Ok(answered(200, "{}")));
            world
                .domains()
                .set_proxied(&world.owner, &host("shop.apps.test"), true)
                .await
                .unwrap();
            let requests: Vec<(String, String, String, String)> = world
                .ports
                .hooks
                .route_requests()
                .into_iter()
                .map(|(module, request)| {
                    (
                        module.to_string(),
                        request.method,
                        request.path,
                        String::from_utf8(request.body).unwrap(),
                    )
                })
                .collect();
            assert_eq!(
                requests,
                vec![(
                    "dns.cloudflare".to_owned(),
                    "PATCH".to_owned(),
                    "/records/shop.apps.test".to_owned(),
                    r#"{"proxied":true}"#.to_owned()
                )]
            );
        }

        // TC: DNS-05
        #[tokio::test]
        async fn stores_flag_when_module_answers_200() {
            let world = World::new().await;
            world.ports.hooks.answer_route(Ok(answered(200, "{}")));
            let set = world
                .domains()
                .set_proxied(&world.owner, &host("shop.apps.test"), true)
                .await
                .unwrap();
            assert_eq!(
                (
                    set.domain.proxied,
                    set.project.to_string(),
                    stored_flag(&world).await
                ),
                (true, "shop".to_owned(), true)
            );
        }

        // TC: DNS-05 (a domain added before the module: it keeps no record)
        #[tokio::test]
        async fn validation_when_module_has_no_record() {
            let world = World::new().await;
            world.ports.hooks.answer_route(Ok(answered(
                404,
                r#"{"status":404,"detail":"no DNS record"}"#,
            )));
            let err = world
                .domains()
                .set_proxied(&world.owner, &host("shop.apps.test"), true)
                .await
                .unwrap_err();
            assert!(
                matches!(&err, Error::Validation(v) if v.field == "host"),
                "{err:?}"
            );
        }

        // TC: DNS-05 (the problem's detail, never the module's raw body)
        #[tokio::test]
        async fn upstream_with_module_detail_when_module_answers_502() {
            let world = World::new().await;
            world.ports.hooks.answer_route(Ok(answered(
                502,
                r#"{"status":502,"detail":"Cloudflare: Invalid access token"}"#,
            )));
            let err = world
                .domains()
                .set_proxied(&world.owner, &host("shop.apps.test"), true)
                .await
                .unwrap_err();
            assert!(
                matches!(
                    &err,
                    Error::Upstream { system: System::Dns, detail }
                        if detail == "dns.cloudflare answered 502: Cloudflare: Invalid access token"
                ),
                "{err:?}"
            );
        }

        // TC: DNS-05 (a body that is no problem object stays in the log)
        #[tokio::test]
        async fn upstream_without_body_when_module_answers_no_problem() {
            let world = World::new().await;
            world
                .ports
                .hooks
                .answer_route(Ok(answered(500, "wasm backtrace: frame 0x1f")));
            let err = world
                .domains()
                .set_proxied(&world.owner, &host("shop.apps.test"), true)
                .await
                .unwrap_err();
            assert!(
                matches!(
                    &err,
                    Error::Upstream { system: System::Dns, detail }
                        if detail == "dns.cloudflare answered 500"
                ),
                "{err:?}"
            );
        }

        // TC: DNS-05 (the row follows the record only once the record changed)
        #[tokio::test]
        async fn keeps_flag_when_module_answers_502() {
            let world = World::new().await;
            world
                .ports
                .hooks
                .answer_route(Ok(answered(502, r#"{"status":502,"detail":"down"}"#)));
            let _refused = world
                .domains()
                .set_proxied(&world.owner, &host("shop.apps.test"), true)
                .await;
            assert!(!stored_flag(&world).await);
        }

        // TC: DNS-05 (HOOK-05: a disabled module)
        #[tokio::test]
        async fn module_required_when_module_disabled() {
            let world = World::new().await;
            world
                .ports
                .hooks
                .answer_route(Err(Error::module_required("dns.cloudflare")));
            let err = world
                .domains()
                .set_proxied(&world.owner, &host("shop.apps.test"), true)
                .await
                .unwrap_err();
            assert!(
                matches!(&err, Error::ModuleRequired(module) if module == "dns.cloudflare")
                    && !stored_flag(&world).await,
                "{err:?}"
            );
        }

        // TC: DNS-05 (compensation: a row that cannot follow puts
        // the record's flag back)
        #[tokio::test]
        async fn puts_record_flag_back_when_storing_flag_fails() {
            let world = World::new().await;
            world.ports.hooks.answer_route(Ok(answered(200, "{}")));
            world.ports.hooks.answer_route(Ok(answered(200, "{}")));
            world
                .ports
                .store
                .fail_next("update_domain", Error::internal_msg("disk full"));
            let result = world
                .domains()
                .set_proxied(&world.owner, &host("shop.apps.test"), true)
                .await;
            let bodies: Vec<String> = world
                .ports
                .hooks
                .route_requests()
                .into_iter()
                .map(|(_, request)| String::from_utf8(request.body).unwrap())
                .collect();
            assert!(
                result.is_err()
                    && bodies == [r#"{"proxied":true}"#, r#"{"proxied":false}"#]
                    && !stored_flag(&world).await,
                "{result:?} {bodies:?}"
            );
        }
    }

    mod reverify_external {
        use super::*;

        // TC: DOM-07
        #[tokio::test]
        async fn marks_record_lost_when_record_disappeared() {
            let world = World::new().await;
            let domain = verified_external(&world).await;
            world.ports.clock.set(at(DAY_TWO));
            world.domains().reverify_external().await.unwrap();
            let stored = world.ports.store.domain(domain.id).await.unwrap().unwrap();
            assert_eq!(
                (stored.record_lost_at, stored.verified_at),
                (Some(at(DAY_TWO)), Some(at(DAY_ONE)))
            );
        }

        // TC: DOM-07 (the route stays)
        #[tokio::test]
        async fn keeps_route_when_record_disappeared() {
            let world = World::new().await;
            verified_external(&world).await;
            world.ports.clock.set(at(DAY_TWO));
            world.domains().reverify_external().await.unwrap();
            let config = ProxyConfig::from_store(&world.ports.store, &world.settings)
                .await
                .unwrap();
            assert_eq!(
                route_of(Some(config), "www.shop.example"),
                Some(Route::Unavailable)
            );
        }

        // TC: DOM-07 (the warning keeps the time the record was first missed)
        #[tokio::test]
        async fn keeps_first_loss_time_when_still_missing() {
            let world = World::new().await;
            let domain = verified_external(&world).await;
            world.ports.clock.set(at(DAY_TWO));
            world.domains().reverify_external().await.unwrap();
            world.ports.clock.set(at(DAY_TWO + 86_400));
            world.domains().reverify_external().await.unwrap();
            let stored = world.ports.store.domain(domain.id).await.unwrap().unwrap();
            assert_eq!(stored.record_lost_at, Some(at(DAY_TWO)));
        }

        // TC: DOM-07 (a record that is back clears the warning)
        #[tokio::test]
        async fn clears_warning_and_renews_when_record_back() {
            let world = World::new().await;
            let mut domain = verified_external(&world).await;
            domain.record_lost_at = Some(at(DAY_ONE));
            world.ports.store.update_domain(&domain).await.unwrap();
            world
                .ports
                .resolver
                .set_cname(&host("www.shop.example"), vec![host("apps.test")]);
            world.ports.clock.set(at(DAY_TWO));
            world.domains().reverify_external().await.unwrap();
            let stored = world.ports.store.domain(domain.id).await.unwrap().unwrap();
            assert_eq!(
                (stored.record_lost_at, stored.verified_at),
                (None, Some(at(DAY_TWO)))
            );
        }

        // TC: DOM-07
        #[tokio::test]
        async fn returns_external_domains_when_reverified() {
            let world = World::new().await;
            verified_external(&world).await;
            world.ports.clock.set(at(DAY_TWO));
            let checked: Vec<(String, Option<Timestamp>)> = world
                .domains()
                .reverify_external()
                .await
                .unwrap()
                .into_iter()
                .map(|d| (d.host.to_string(), d.record_lost_at))
                .collect();
            assert_eq!(
                checked,
                vec![("www.shop.example".to_owned(), Some(at(DAY_TWO)))]
            );
        }

        // TC: DOM-07 (managed domains live under the wildcard: never resolved)
        #[tokio::test]
        async fn resolves_only_external_hosts_when_reverifying() {
            let world = World::new().await;
            verified_external(&world).await;
            world.log().clear();
            world.domains().reverify_external().await.unwrap();
            let asked: Vec<Host> = world
                .log()
                .calls()
                .into_iter()
                .filter_map(|call| match call {
                    Call::Resolve(ResolveCall::Cname(h) | ResolveCall::A(h)) => Some(h),
                    _ => None,
                })
                .collect();
            assert!(
                !asked.is_empty() && asked.iter().all(|h| h.as_str() == "www.shop.example"),
                "{asked:?}"
            );
        }

        // TC: DOM-07 (a resolver failure is not a lost record)
        #[tokio::test]
        async fn keeps_domain_unchanged_when_resolver_fails() {
            let world = World::new().await;
            let domain = verified_external(&world).await;
            world
                .ports
                .resolver
                .fail_always("cname", Error::internal_msg("SERVFAIL"));
            world
                .ports
                .resolver
                .fail_always("a", Error::internal_msg("SERVFAIL"));
            world.ports.clock.set(at(DAY_TWO));
            let _outcome = world.domains().reverify_external().await;
            let stored = world.ports.store.domain(domain.id).await.unwrap().unwrap();
            assert_eq!(stored, domain);
        }
    }

    mod wildcard_dns {
        use super::*;

        const PUBLIC_IP: Ipv4Addr = Ipv4Addr::new(203, 0, 113, 10);

        // TC: CLIX-09 (`doctor` checks wildcard DNS)
        #[tokio::test]
        async fn points_at_us_when_a_fresh_label_resolves_to_public_ip() {
            let world = World::new().await;
            world
                .ports
                .resolver
                .set_wildcard_a(&host("apps.test"), vec![PUBLIC_IP]);
            let report = world.domains().wildcard_dns().await.unwrap();
            assert_eq!(
                (
                    report.verdict,
                    report.probe.label_in(&host("apps.test")).is_some(),
                    report.base,
                    report.public_ip
                ),
                (
                    WildcardVerdict::PointsAtUs,
                    true,
                    host("apps.test"),
                    PUBLIC_IP
                )
            );
        }

        // TC: CLIX-09
        #[tokio::test]
        async fn elsewhere_when_a_fresh_label_resolves_to_another_ip() {
            let world = World::new().await;
            let other = Ipv4Addr::new(198, 51, 100, 7);
            world
                .ports
                .resolver
                .set_wildcard_a(&host("apps.test"), vec![other]);
            let report = world.domains().wildcard_dns().await.unwrap();
            assert_eq!(report.verdict, WildcardVerdict::Elsewhere(vec![other]));
        }

        // TC: CLIX-09 (an explicit record is not the wildcard)
        #[tokio::test]
        async fn missing_when_only_an_explicit_name_resolves() {
            let world = World::new().await;
            world
                .ports
                .resolver
                .set_a(&host("www.apps.test"), vec![PUBLIC_IP]);
            let report = world.domains().wildcard_dns().await.unwrap();
            assert_eq!(report.verdict, WildcardVerdict::Missing);
        }

        // TC: CLIX-09 (a fresh label each time: no cached answer)
        #[tokio::test]
        async fn asks_a_fresh_label_when_run_twice() {
            let world = World::new().await;
            let first = world.domains().wildcard_dns().await.unwrap();
            let second = world.domains().wildcard_dns().await.unwrap();
            assert_ne!(first.probe, second.probe);
        }

        // TC: CLIX-09
        #[tokio::test]
        async fn passes_the_error_on_when_the_resolver_fails() {
            let world = World::new().await;
            world
                .ports
                .resolver
                .fail_next("a", Error::upstream(System::Dns, "SERVFAIL"));
            let error = world.domains().wildcard_dns().await.unwrap_err();
            assert!(matches!(error, Error::Upstream { .. }), "{error:?}");
        }
    }
}
