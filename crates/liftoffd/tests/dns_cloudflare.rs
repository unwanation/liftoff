#![cfg(test)]
//! The `dns.cloudflare` module (DNS-01…05): the
//! built `modules/dns-cloudflare` component loaded by the module host,
//! the core's `ModuleHooks` over it, and the core's `DomainService` over
//! the world with those hooks, so the domain hooks and `set --proxied`
//! run the way the daemon runs them.
//!
//! The module keeps its records in its table `m_dns_cloudflare_records`
//! through `store-query`, which needs SQLite: its host stores in a
//! `SqliteStore` (the core's rows stay in the world's `InMemoryStore`),
//! which is why the test lives with the composition root (it joins
//! `liftoff-wasm` and `liftoff-store`, which never depend on each
//! other). Its `wasi:http` reaches `api.cloudflare.com` (its
//! `http_allow`), which the host routes to a fake Cloudflare v4 DNS API
//! served by this test on loopback (`ModuleHost::routing_https_to`).
//!
//! The component is built by `just modules` (part of `just test` and `just
//! ci`); `liftoff_wasm::testing::component` reads it from
//! `WASM_MODULES_DIR`, which the just recipes set, else from
//! `modules/target/wasm32-wasip2/release/`.
//!
//! The flow these tests pin is v1's reservation moved into
//! the hooks: `domain_created` of a managed domain looks the name up
//! (`GET dns_records?name=`), denies when a record exists that liftoff did
//! not create, else creates the A record (`POST dns_records`) and stores
//! its id; when storing the id fails it deletes the record again and
//! denies, so the core takes the domain back. `domain_removed` deletes by
//! the stored id only. `DomainService::set_proxied` hands the flag to the
//! module's route `PATCH /records/{host}`, which patches the record.

use core::net::SocketAddr;
use core::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use liftoff_core::Error;
use liftoff_core::domain::{DomainKind, Host, Label, ModuleName, ProjectName};
use liftoff_core::error::System;
use liftoff_core::fakes::{WithHooks, World};
use liftoff_core::module::{ModuleCalls as _, ModuleHooks};
use liftoff_core::ports::{HookResult, Hooks, ModuleRequest, ModuleStore, Store as _};
use liftoff_core::service::DomainService;
use liftoff_store::SqliteStore;
use liftoff_wasm::testing::{Host as ModuleHostOf, component, limits};
use liftoff_wasm::{ModuleConfig, ModuleSet, TokioTimer, TracingLog};
use serde_json::{Value, json};

/// The zone the module is configured with.
const ZONE: &str = "z1";
/// The API token the module is configured with (a secret of its config).
const TOKEN: &str = "cf-token";
/// The DNS records of [`ZONE`] as Cloudflare's v4 API names them.
const RECORDS: &str = "/client/v4/zones/z1/dns_records";
/// The managed domain the tests add: `blog` under the world's base.
const BLOG: &str = "blog.apps.test";

fn dns() -> ModuleName {
    ModuleName::new("dns.cloudflare").unwrap()
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// --- The fake Cloudflare API --------------------------------------------------

/// A DNS record of the fake zone.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Record {
    id: String,
    name: String,
    content: String,
    proxied: bool,
}

/// A request the fake received.
#[derive(Debug, Clone)]
struct Seen {
    method: String,
    /// Path and query.
    target: String,
    authorization: Option<String>,
    body: Value,
}

#[derive(Debug, Default)]
struct ZoneState {
    records: Vec<Record>,
    created: usize,
    seen: Vec<Seen>,
    /// Methods whose next request fails with a `500` envelope.
    failing: Vec<String>,
}

/// The message of the `500` envelope [`Cloudflare::fail_next`] answers.
const OUTAGE: &str = "Internal server error";

/// Cloudflare's v4 DNS records API of [`ZONE`] on `127.0.0.1`: list by
/// `name`, create (ids `rec-1`, `rec-2`, …), patch and delete by id, each
/// in the `{success, errors, result}` envelope.
#[derive(Debug, Clone)]
struct Cloudflare {
    addr: SocketAddr,
    state: Arc<Mutex<ZoneState>>,
}

impl Cloudflare {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let fake = Self {
            addr,
            state: Arc::new(Mutex::new(ZoneState::default())),
        };
        let app = axum::Router::new()
            .fallback(handle)
            .with_state(fake.clone());
        drop(tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }));
        fake
    }

    /// A record of `name` that liftoff did not create.
    fn foreign(&self, name: &str) {
        lock(&self.state).records.push(Record {
            id: "foreign-1".to_owned(),
            name: name.to_owned(),
            content: "198.51.100.1".to_owned(),
            proxied: false,
        });
    }

    fn records(&self) -> Vec<Record> {
        lock(&self.state).records.clone()
    }

    /// The ids of the zone's records.
    fn ids(&self) -> Vec<String> {
        self.records().into_iter().map(|record| record.id).collect()
    }

    /// The next request of `method` fails with a `500` envelope
    /// ([`OUTAGE`]) and changes nothing.
    fn fail_next(&self, method: &str) {
        lock(&self.state).failing.push(method.to_owned());
    }

    /// Deletes the record `id` behind liftoff's back.
    fn forget(&self, id: &str) {
        lock(&self.state).records.retain(|record| record.id != id);
    }

    fn seen(&self) -> Vec<Seen> {
        lock(&self.state).seen.clone()
    }

    /// Method and target of every request seen, in order.
    fn requests(&self) -> Vec<(String, String)> {
        self.seen()
            .into_iter()
            .map(|seen| (seen.method, seen.target))
            .collect()
    }

    /// Forgets the requests seen so far.
    fn clear_seen(&self) {
        lock(&self.state).seen.clear();
    }

    /// The one request of `method`.
    fn only(&self, method: &str) -> Seen {
        let mut matching: Vec<Seen> = self
            .seen()
            .into_iter()
            .filter(|seen| seen.method == method)
            .collect();
        assert_eq!(matching.len(), 1, "{:?}", self.requests());
        matching.remove(0)
    }
}

fn envelope(status: StatusCode, result: &Value) -> (StatusCode, axum::Json<Value>) {
    let success = status.is_success();
    let errors = if success {
        json!([])
    } else if status == StatusCode::NOT_FOUND {
        json!([{"code": 81044, "message": "Record does not exist."}])
    } else {
        json!([{"code": 10000, "message": OUTAGE}])
    };
    (
        status,
        axum::Json(json!({"success": success, "errors": errors, "messages": [], "result": result})),
    )
}

fn record_json(record: &Record) -> Value {
    json!({
        "id": record.id, "type": "A", "name": record.name,
        "content": record.content, "proxied": record.proxied, "ttl": 1,
    })
}

async fn handle(
    State(fake): State<Cloudflare>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, axum::Json<Value>) {
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let seen = Seen {
        method: method.to_string(),
        target: uri
            .path_and_query()
            .map_or_else(|| uri.path().to_owned(), ToString::to_string),
        authorization: headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned),
        body: body.clone(),
    };
    answer(&mut lock(&fake.state), seen, &method, &uri, &body)
}

/// What the zone answers `method uri` with `body`, `seen` recorded.
fn answer(
    state: &mut ZoneState,
    seen: Seen,
    method: &Method,
    uri: &Uri,
    body: &Value,
) -> (StatusCode, axum::Json<Value>) {
    state.seen.push(seen);
    if let Some(at) = state
        .failing
        .iter()
        .position(|failing| failing == method.as_str())
    {
        drop(state.failing.remove(at));
        return envelope(StatusCode::INTERNAL_SERVER_ERROR, &Value::Null);
    }
    let path = uri.path();
    let id = path
        .strip_prefix(RECORDS)
        .and_then(|rest| rest.strip_prefix('/'))
        .map(str::to_owned);
    match (method.as_str(), id) {
        ("GET", None) if path == RECORDS => {
            let name = uri
                .query()
                .unwrap_or_default()
                .split('&')
                .find_map(|pair| pair.strip_prefix("name="))
                .unwrap_or_default()
                .to_owned();
            let found: Vec<Value> = state
                .records
                .iter()
                .filter(|record| record.name == name)
                .map(record_json)
                .collect();
            envelope(StatusCode::OK, &Value::Array(found))
        }
        ("POST", None) if path == RECORDS => {
            state.created = state.created.saturating_add(1);
            let record = Record {
                id: format!("rec-{}", state.created),
                name: body["name"].as_str().unwrap_or_default().to_owned(),
                content: body["content"].as_str().unwrap_or_default().to_owned(),
                proxied: body["proxied"].as_bool().unwrap_or(false),
            };
            let answer = record_json(&record);
            state.records.push(record);
            envelope(StatusCode::OK, &answer)
        }
        ("PATCH", Some(id)) => {
            let Some(record) = state.records.iter_mut().find(|record| record.id == id) else {
                return envelope(StatusCode::NOT_FOUND, &Value::Null);
            };
            if let Some(proxied) = body["proxied"].as_bool() {
                record.proxied = proxied;
            }
            let answer = record_json(record);
            envelope(StatusCode::OK, &answer)
        }
        ("DELETE", Some(id)) => {
            let before = state.records.len();
            state.records.retain(|record| record.id != id);
            if state.records.len() == before {
                return envelope(StatusCode::NOT_FOUND, &Value::Null);
            }
            envelope(StatusCode::OK, &json!({"id": id}))
        }
        _ => envelope(StatusCode::NOT_FOUND, &Value::Null),
    }
}

// --- The module's store -------------------------------------------------------

/// The daemon's SQLite store as the module host sees it, whose writes
/// through `store-query` fail while armed (a full disk after the record
/// was created: DNS-03). Reads keep working.
#[derive(Debug, Clone)]
struct ModuleTables {
    sqlite: SqliteStore,
    fail_writes: Arc<AtomicBool>,
}

impl ModuleTables {
    fn new() -> Self {
        Self {
            sqlite: SqliteStore::open_in_memory().unwrap(),
            fail_writes: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Every write of `store-query` fails from now on.
    fn fail_writes(&self) {
        self.fail_writes.store(true, Ordering::SeqCst);
    }

    /// The module's records: `host` and `cf_record_id` of every row.
    async fn rows(&self) -> Value {
        let rows = self
            .sqlite
            .module_query(
                &dns(),
                "SELECT host, cf_record_id FROM m_dns_cloudflare_records ORDER BY host",
                &[],
            )
            .await
            .unwrap();
        serde_json::from_str(&rows).unwrap()
    }

    /// The `domain_id` kept for `host`.
    async fn domain_id_of(&self, host: &str) -> Option<String> {
        let rows = self
            .sqlite
            .module_query(
                &dns(),
                "SELECT domain_id FROM m_dns_cloudflare_records WHERE host = ?1",
                &[host.to_owned()],
            )
            .await
            .unwrap();
        let rows: Value = serde_json::from_str(&rows).unwrap();
        rows[0]["domain_id"].as_str().map(str::to_owned)
    }
}

impl ModuleStore for ModuleTables {
    fn module_get(
        &self,
        module: &ModuleName,
        key: &str,
    ) -> impl Future<Output = Result<Option<Vec<u8>>, Error>> + Send {
        self.sqlite.module_get(module, key)
    }

    fn module_set(
        &self,
        module: &ModuleName,
        key: &str,
        value: &[u8],
    ) -> impl Future<Output = Result<(), Error>> + Send {
        self.sqlite.module_set(module, key, value)
    }

    async fn module_query(
        &self,
        module: &ModuleName,
        sql: &str,
        params: &[String],
    ) -> Result<String, Error> {
        let reads = sql.trim_start().to_ascii_uppercase().starts_with("SELECT");
        if self.fail_writes.load(Ordering::SeqCst) && !reads {
            return Err(Error::internal_msg("database or disk is full"));
        }
        self.sqlite.module_query(module, sql, params).await
    }

    fn module_purge(&self, module: &ModuleName) -> impl Future<Output = Result<(), Error>> + Send {
        self.sqlite.module_purge(module)
    }
}

// --- The module under test -------------------------------------------------------

type Dns = ModuleHooks<ModuleSet<ModuleTables, TracingLog>>;

/// The module, the domain service over the world with its hooks (`H`
/// over the module's), the fake API and the module's tables.
struct Fixture<H = Dns> {
    world: World,
    ports: Arc<WithHooks<H>>,
    domains: DomainService<WithHooks<H>>,
    cloudflare: Cloudflare,
    tables: ModuleTables,
}

/// The module's hooks, then a module installed after it that denies
/// `domain_created` (a quota, say): what the core must take back.
struct LaterDenies(Dns);

impl Hooks for LaterDenies {
    async fn domain_created(&self, domain: &liftoff_core::domain::Domain) -> HookResult {
        match self.0.domain_created(domain).await {
            HookResult::Deny(reason) => HookResult::Deny(reason),
            HookResult::Continue | HookResult::Allow => {
                HookResult::Deny("a later module denies".to_owned())
            }
        }
    }

    fn domain_removed(
        &self,
        domain: &liftoff_core::domain::Domain,
    ) -> impl Future<Output = HookResult> + Send {
        self.0.domain_removed(domain)
    }
}

/// The module's configuration: `zone_id`, `api_token` (a secret,
/// decrypted by the host) and `ip`, the world's public IP.
fn config() -> ModuleConfig {
    [
        ("zone_id", ZONE.to_owned()),
        ("api_token", TOKEN.to_owned()),
        ("ip", World::PUBLIC_IP.to_string()),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_owned(), value))
    .collect()
}

/// The built `dns.cloudflare` component loaded and migrated from 0, as an
/// install does, its `api.cloudflare.com` routed to a fresh fake.
async fn fixture() -> Fixture {
    fixture_with(config(), |dns| dns).await
}

/// [`fixture`] configured with `config`, its hooks `wrap`ped.
async fn fixture_with<H: Hooks + 'static>(
    config: ModuleConfig,
    wrap: impl FnOnce(Dns) -> H,
) -> Fixture<H> {
    let world = World::new().await;
    let cloudflare = Cloudflare::start().await;
    let tables = ModuleTables::new();
    let host: ModuleHostOf<ModuleTables> =
        ModuleHostOf::new(tables.clone(), TokioTimer, TracingLog, limits())
            .unwrap()
            .routing_https_to("api.cloudflare.com", cloudflare.addr);
    let module = host
        .load(dns(), &component("dns_cloudflare").unwrap(), config)
        .await
        .unwrap();
    module.migrate(0).await.unwrap();
    let set = ModuleSet::new();
    set.insert(module);
    let hooks = wrap(ModuleHooks::new(Arc::clone(host.registry()), set));
    let ports = Arc::new(WithHooks::new(Arc::clone(&world.ports), hooks));
    let domains = DomainService::new(Arc::clone(&ports), Arc::clone(&world.settings));
    Fixture {
        world,
        ports,
        domains,
        cloudflare,
        tables,
    }
}

impl<H: Hooks + 'static> Fixture<H> {
    /// `lof domain add blog` in project `shop` as its owner.
    async fn add_blog(&self) -> Result<liftoff_core::domain::Domain, Error> {
        self.add_blog_proxied(false).await
    }

    /// `lof domain add blog [--proxied]` in project `shop` as its owner.
    async fn add_blog_proxied(&self, proxied: bool) -> Result<liftoff_core::domain::Domain, Error> {
        self.domains
            .add_managed(
                &self.world.owner,
                &ProjectName::new("shop").unwrap(),
                Label::new("blog").unwrap(),
                proxied,
            )
            .await
    }

    /// `lof domain rm blog.apps.test` as its owner.
    async fn remove_blog(&self) -> Result<(), Error> {
        self.domains
            .remove(&self.world.owner, &Host::new(BLOG).unwrap())
            .await
    }

    /// Whether the core stores a domain `host`.
    async fn stored(&self, host: &str) -> Option<liftoff_core::domain::Domain> {
        self.world
            .ports
            .store
            .domain_by_host(&Host::new(host).unwrap())
            .await
            .unwrap()
    }
}

fn target(suffix: &str) -> String {
    format!("{RECORDS}{suffix}")
}

fn request(method: &str, target: String) -> (String, String) {
    (method.to_owned(), target)
}

mod manifest {
    use super::*;

    // TC: DNS-01 (validate at the boundary: `module configure` refuses an
    // `ip` that is not an IPv4 address)
    #[tokio::test(flavor = "multi_thread")]
    async fn config_schema_accepts_only_ipv4_as_ip() {
        let fixture = fixture().await;
        let schema = fixture
            .ports
            .hooks
            .registry()
            .manifest(&dns())
            .and_then(|manifest| manifest.config_schema)
            .unwrap();
        let validator = jsonschema::validator_for(&schema).unwrap();
        let accepted: Vec<bool> = [
            "203.0.113.10",
            "not-an-ip",
            "256.1.1.1",
            "1.2.3",
            "2001:db8::1",
        ]
        .into_iter()
        .map(|ip| validator.is_valid(&json!({"zone_id": ZONE, "api_token": TOKEN, "ip": ip})))
        .collect();
        assert_eq!(accepted, vec![true, false, false, false, false]);
    }
}

mod domain_created {
    use super::*;

    // TC: DNS-01 (the name is looked up before anything is created, v1 7.1 step 3)
    #[tokio::test(flavor = "multi_thread")]
    async fn checks_name_then_creates_record_when_managed_domain_added() {
        let fixture = fixture().await;
        fixture.add_blog().await.unwrap();
        assert_eq!(
            fixture.cloudflare.requests(),
            vec![
                request("GET", target("?name=blog.apps.test")),
                request("POST", target("")),
            ]
        );
    }

    // TC: DNS-01 (v1 7.1 step 4: A to the public IP, ttl automatic, the domain's flag)
    #[tokio::test(flavor = "multi_thread")]
    async fn posts_a_record_to_public_ip_when_managed_domain_added() {
        let fixture = fixture().await;
        fixture.add_blog().await.unwrap();
        let posted = fixture.cloudflare.only("POST");
        assert_eq!(
            (posted.body, posted.authorization.as_deref()),
            (
                json!({"type": "A", "name": BLOG, "content": "203.0.113.10", "ttl": 1, "proxied": false}),
                Some("Bearer cf-token"),
            )
        );
    }

    // TC: DNS-01 (the record's id lands in the module's table)
    #[tokio::test(flavor = "multi_thread")]
    async fn stores_record_id_when_record_created() {
        let fixture = fixture().await;
        fixture.add_blog().await.unwrap();
        assert_eq!(
            fixture.tables.rows().await,
            json!([{"host": BLOG, "cf_record_id": "rec-1"}])
        );
    }

    // TC: DNS-01 (records for managed domains only; an external domain's DNS is its owner's)
    #[tokio::test(flavor = "multi_thread")]
    async fn creates_no_record_when_domain_external() {
        let fixture = fixture().await;
        let shop = Host::new("shop.example.org").unwrap();
        fixture
            .world
            .ports
            .resolver
            .set_cname(&shop, vec![Host::new(World::BASE).unwrap()]);
        let added = fixture
            .domains
            .add_external(
                &fixture.world.owner,
                &ProjectName::new("shop").unwrap(),
                shop,
            )
            .await
            .unwrap();
        assert_eq!(
            (added.kind, fixture.cloudflare.requests()),
            (DomainKind::External, Vec::new())
        );
    }

    // TC: DNS-02 (v1 7.1 step 3: a record liftoff did not create is never touched)
    #[tokio::test(flavor = "multi_thread")]
    async fn denies_when_name_taken_outside_liftoff() {
        let fixture = fixture().await;
        fixture.cloudflare.foreign(BLOG);
        let refused = fixture.add_blog().await;
        assert!(
            matches!(&refused, Err(Error::Forbidden(reason)) if reason.contains("outside liftoff")),
            "{refused:?}"
        );
    }

    // TC: DNS-02
    #[tokio::test(flavor = "multi_thread")]
    async fn stores_no_domain_when_name_taken_outside_liftoff() {
        let fixture = fixture().await;
        fixture.cloudflare.foreign(BLOG);
        let _refused = fixture.add_blog().await;
        assert!(fixture.stored(BLOG).await.is_none());
    }

    // TC: DNS-02
    #[tokio::test(flavor = "multi_thread")]
    async fn leaves_foreign_record_when_name_taken_outside_liftoff() {
        let fixture = fixture().await;
        fixture.cloudflare.foreign(BLOG);
        let _refused = fixture.add_blog().await;
        assert_eq!(
            (
                fixture.cloudflare.requests(),
                fixture
                    .cloudflare
                    .records()
                    .into_iter()
                    .map(|record| record.id)
                    .collect::<Vec<_>>(),
            ),
            (
                vec![request("GET", target("?name=blog.apps.test"))],
                vec!["foreign-1".to_owned()],
            )
        );
    }

    // TC: DNS-01 (`domain add --proxied` creates the record proxied)
    #[tokio::test(flavor = "multi_thread")]
    async fn posts_proxied_record_when_added_proxied() {
        let fixture = fixture().await;
        let added = fixture.add_blog_proxied(true).await.unwrap();
        let posted = fixture.cloudflare.only("POST");
        assert_eq!(
            (
                added.proxied,
                posted.body["proxied"].as_bool(),
                fixture
                    .cloudflare
                    .records()
                    .first()
                    .map(|record| record.proxied)
            ),
            (true, Some(true), Some(true))
        );
    }

    // TC: DNS-01 (a failed create leaves neither a domain nor a row; the
    // deny carries Cloudflare's message)
    #[tokio::test(flavor = "multi_thread")]
    async fn denies_and_stores_nothing_when_cloudflare_create_fails() {
        let fixture = fixture().await;
        fixture.cloudflare.fail_next("POST");
        let refused = fixture.add_blog().await;
        assert!(
            matches!(&refused, Err(Error::Forbidden(reason)) if reason.contains(OUTAGE))
                && fixture.stored(BLOG).await.is_none()
                && fixture.tables.rows().await == json!([]),
            "{refused:?} {}",
            fixture.tables.rows().await
        );
    }

    // TC: DNS-01 (v1 7.1 step 3: no lookup, no record)
    #[tokio::test(flavor = "multi_thread")]
    async fn creates_nothing_when_cloudflare_lookup_fails() {
        let fixture = fixture().await;
        fixture.cloudflare.fail_next("GET");
        let refused = fixture.add_blog().await;
        assert!(
            matches!(&refused, Err(Error::Forbidden(_)))
                && fixture.stored(BLOG).await.is_none()
                && fixture.cloudflare.requests()
                    == vec![request("GET", target("?name=blog.apps.test"))],
            "{refused:?} {:?}",
            fixture.cloudflare.requests()
        );
    }

    // TC: DNS-01 (validate at the boundary: `ip` must be an IPv4 address)
    #[tokio::test(flavor = "multi_thread")]
    async fn denies_without_calling_cloudflare_when_ip_not_ipv4() {
        let mut bad = config();
        let _previous = bad.insert("ip".to_owned(), "not-an-ip".to_owned());
        let fixture = fixture_with(bad, |dns| dns).await;
        let refused = fixture.add_blog().await;
        assert!(
            matches!(&refused, Err(Error::Forbidden(reason)) if reason.contains("ip"))
                && fixture.cloudflare.requests().is_empty(),
            "{refused:?} {:?}",
            fixture.cloudflare.requests()
        );
    }

    // TC: DNS-02 (the record liftoff created and failed to delete is
    // liftoff's own: a re-add takes it over instead of denying)
    #[tokio::test(flavor = "multi_thread")]
    async fn takes_over_own_record_when_earlier_delete_failed() {
        let fixture = fixture().await;
        fixture.add_blog().await.unwrap();
        fixture.cloudflare.fail_next("DELETE");
        fixture.remove_blog().await.unwrap();
        fixture.cloudflare.clear_seen();
        let again = fixture.add_blog().await.unwrap();
        assert_eq!(
            (
                fixture.cloudflare.requests(),
                fixture.cloudflare.ids(),
                fixture.tables.rows().await,
                fixture.tables.domain_id_of(BLOG).await,
            ),
            (
                vec![request("GET", target("?name=blog.apps.test"))],
                vec!["rec-1".to_owned()],
                json!([{"host": BLOG, "cf_record_id": "rec-1"}]),
                Some(again.id.to_string()),
            )
        );
    }

    // TC: DNS-01 (a module after dns.cloudflare denies, the core takes the domain back and `domain_removed` deletes the new record)
    #[tokio::test(flavor = "multi_thread")]
    async fn deletes_new_record_when_later_module_denies() {
        let fixture = fixture_with(config(), LaterDenies).await;
        let refused = fixture.add_blog().await;
        assert_eq!(
            (
                matches!(refused, Err(Error::Forbidden(_))),
                fixture.cloudflare.requests(),
                fixture.cloudflare.ids(),
                fixture.tables.rows().await,
            ),
            (
                true,
                vec![
                    request("GET", target("?name=blog.apps.test")),
                    request("POST", target("")),
                    request("DELETE", target("/rec-1")),
                ],
                Vec::<String>::new(),
                json!([]),
            )
        );
    }

    // TC: DNS-03 (v1 7.1 step 5: the record is deleted when its id cannot be stored)
    #[tokio::test(flavor = "multi_thread")]
    async fn deletes_created_record_when_storing_id_fails() {
        let fixture = fixture().await;
        fixture.tables.fail_writes();
        let _refused = fixture.add_blog().await;
        assert_eq!(
            (fixture.cloudflare.requests(), fixture.cloudflare.records()),
            (
                vec![
                    request("GET", target("?name=blog.apps.test")),
                    request("POST", target("")),
                    request("DELETE", target("/rec-1")),
                ],
                Vec::new(),
            )
        );
    }

    // TC: DNS-03 (the hook denies, so the core takes the domain back)
    #[tokio::test(flavor = "multi_thread")]
    async fn stores_no_domain_when_storing_id_fails() {
        let fixture = fixture().await;
        fixture.tables.fail_writes();
        let refused = fixture.add_blog().await;
        assert!(
            matches!(refused, Err(Error::Forbidden(_))) && fixture.stored(BLOG).await.is_none(),
            "{refused:?}"
        );
    }
}

mod domain_removed {
    use super::*;

    // TC: DNS-04 (v1 7.1: the record goes by its stored id)
    #[tokio::test(flavor = "multi_thread")]
    async fn deletes_record_by_stored_id_when_domain_removed() {
        let fixture = fixture().await;
        let blog = fixture.add_blog().await.unwrap();
        fixture.cloudflare.clear_seen();
        fixture
            .domains
            .remove(&fixture.world.owner, &blog.host)
            .await
            .unwrap();
        assert_eq!(
            (fixture.cloudflare.requests(), fixture.cloudflare.records()),
            (vec![request("DELETE", target("/rec-1"))], Vec::new())
        );
    }

    // TC: DNS-04 (foreign records are never touched: only the stored id
    // goes, a foreign record beside it stays)
    #[tokio::test(flavor = "multi_thread")]
    async fn deletes_only_own_record_when_foreign_record_beside_it() {
        let fixture = fixture().await;
        fixture.cloudflare.foreign("other.apps.test");
        fixture.add_blog().await.unwrap();
        fixture.cloudflare.clear_seen();
        fixture.remove_blog().await.unwrap();
        assert_eq!(
            (fixture.cloudflare.requests(), fixture.cloudflare.ids()),
            (
                vec![request("DELETE", target("/rec-1"))],
                vec!["foreign-1".to_owned()]
            )
        );
    }

    // TC: DNS-04 (v1 DOM-08: a record Cloudflare no longer knows counts as deleted)
    #[tokio::test(flavor = "multi_thread")]
    async fn forgets_record_id_when_cloudflare_already_deleted_it() {
        let fixture = fixture().await;
        fixture.add_blog().await.unwrap();
        fixture.cloudflare.forget("rec-1");
        let removed = fixture.remove_blog().await;
        assert_eq!(
            (removed.is_ok(), fixture.tables.rows().await),
            (true, json!([]))
        );
    }

    // TC: DNS-04 (a failed delete keeps the id: the record is still liftoff's)
    #[tokio::test(flavor = "multi_thread")]
    async fn keeps_record_id_when_cloudflare_delete_fails() {
        let fixture = fixture().await;
        fixture.add_blog().await.unwrap();
        fixture.cloudflare.fail_next("DELETE");
        let removed = fixture.remove_blog().await;
        assert_eq!(
            (
                removed.is_ok(),
                fixture.stored(BLOG).await.is_none(),
                fixture.tables.rows().await,
                fixture.cloudflare.ids()
            ),
            (
                true,
                true,
                json!([{"host": BLOG, "cf_record_id": "rec-1"}]),
                vec!["rec-1".to_owned()]
            )
        );
    }

    // TC: DNS-04
    #[tokio::test(flavor = "multi_thread")]
    async fn forgets_record_id_when_domain_removed() {
        let fixture = fixture().await;
        let blog = fixture.add_blog().await.unwrap();
        fixture
            .domains
            .remove(&fixture.world.owner, &blog.host)
            .await
            .unwrap();
        assert_eq!(fixture.tables.rows().await, json!([]));
    }

    // TC: DNS-04 (a domain the module holds no id for: its record is
    // someone else's and stays, whatever its name)
    #[tokio::test(flavor = "multi_thread")]
    async fn leaves_foreign_record_when_domain_has_no_stored_id() {
        let fixture = fixture().await;
        let older = fixture
            .world
            .records
            .domain(&fixture.world.shop, "older.apps.test")
            .unwrap();
        fixture
            .world
            .ports
            .store
            .insert_domain(&older)
            .await
            .unwrap();
        fixture.cloudflare.foreign("older.apps.test");
        fixture
            .domains
            .remove(&fixture.world.owner, &older.host)
            .await
            .unwrap();
        assert_eq!(
            (
                fixture
                    .cloudflare
                    .requests()
                    .into_iter()
                    .filter(|(method, _)| method != "GET")
                    .collect::<Vec<_>>(),
                fixture.cloudflare.records().len(),
            ),
            (Vec::new(), 1)
        );
    }
}

mod set_proxied {
    use super::*;

    // TC: DNS-05 (v1 7.1: `PATCH dns_records/<id> {proxied}`)
    #[tokio::test(flavor = "multi_thread")]
    async fn patches_record_flag_when_set_proxied() {
        let fixture = fixture().await;
        let blog = fixture.add_blog().await.unwrap();
        fixture.cloudflare.clear_seen();
        fixture
            .domains
            .set_proxied(&fixture.world.owner, &blog.host, true)
            .await
            .unwrap();
        let patched = fixture.cloudflare.only("PATCH");
        assert_eq!(
            (patched.target, patched.body),
            (target("/rec-1"), json!({"proxied": true}))
        );
    }

    // TC: DNS-05 (the core keeps the flag `lof domain ls` shows)
    #[tokio::test(flavor = "multi_thread")]
    async fn stores_flag_when_set_proxied() {
        let fixture = fixture().await;
        let blog = fixture.add_blog().await.unwrap();
        let set = fixture
            .domains
            .set_proxied(&fixture.world.owner, &blog.host, true)
            .await
            .unwrap();
        assert_eq!(
            (
                set.domain.proxied,
                fixture.stored(BLOG).await.map(|domain| domain.proxied)
            ),
            (true, Some(true))
        );
    }

    // TC: DNS-05 (Cloudflare's message, not the module's JSON; the row
    // keeps its flag)
    #[tokio::test(flavor = "multi_thread")]
    async fn upstream_with_cloudflare_message_when_patch_fails() {
        let fixture = fixture().await;
        let blog = fixture.add_blog().await.unwrap();
        fixture.cloudflare.fail_next("PATCH");
        let refused = fixture
            .domains
            .set_proxied(&fixture.world.owner, &blog.host, true)
            .await;
        assert!(
            matches!(
                &refused,
                Err(Error::Upstream { system: System::Dns, detail })
                    if detail.contains(OUTAGE) && !detail.contains('{')
            ) && fixture
                .stored(BLOG)
                .await
                .is_some_and(|domain| !domain.proxied),
            "{refused:?}"
        );
    }

    // TC: DNS-05 (the module's route the core hands the flag to)
    #[tokio::test(flavor = "multi_thread")]
    async fn answers_flag_when_route_patched() {
        let fixture = fixture().await;
        let _blog = fixture.add_blog().await.unwrap();
        let answered = fixture
            .ports
            .hooks
            .calls()
            .handle_route(
                &dns(),
                Some(&fixture.world.owner),
                ModuleRequest {
                    method: "PATCH".to_owned(),
                    path: format!("/records/{BLOG}"),
                    headers: vec![("content-type".to_owned(), "application/json".to_owned())],
                    body: json!({"proxied": true}).to_string().into_bytes(),
                },
            )
            .await
            .unwrap();
        assert_eq!(
            (
                answered.status,
                serde_json::from_slice::<Value>(&answered.body).ok()
            ),
            (200, Some(json!({"host": BLOG, "proxied": true})))
        );
    }

    // TC: DNS-05 (the route is the core's way in, after its `authorize`:
    // the manifest does not declare it, so `/api/v1/m/` cannot reach it)
    #[tokio::test(flavor = "multi_thread")]
    async fn not_found_when_route_asked_through_module_api() {
        let fixture = fixture().await;
        let refused = fixture
            .ports
            .hooks
            .module_route(&dns(), "PATCH", &format!("/records/{BLOG}"))
            .await;
        assert!(matches!(refused, Err(Error::NotFound(_))), "{refused:?}");
    }
}

mod reinstall {
    use super::*;

    // TC: REG-07 (the table `remove` kept without `--purge` serves the next install as it is)
    #[tokio::test(flavor = "multi_thread")]
    async fn keeps_records_when_reinstalled_over_kept_table() {
        let fixture = fixture().await;
        fixture.add_blog().await.unwrap();
        let before = fixture.tables.rows().await;
        let host: ModuleHostOf<ModuleTables> =
            ModuleHostOf::new(fixture.tables.clone(), TokioTimer, TracingLog, limits()).unwrap();
        let again = host
            .load(dns(), &component("dns_cloudflare").unwrap(), config())
            .await
            .unwrap();
        let migrated = again.migrate(0).await;
        assert_eq!(
            (migrated.is_ok(), fixture.tables.rows().await),
            (true, before)
        );
    }
}
