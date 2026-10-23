//! `dns.cloudflare`: the module that keeps a Cloudflare A record for every
//! managed domain, carried over
//! from v1's Cloudflare client and its reservation flow.
//!
//! Its configuration: `zone_id`, `api_token` (a secret) and `ip`, the
//! IPv4 address the A records point at (the installation's public IP).
//! `wasi:http` reaches `api.cloudflare.com` only. Its table
//! `m_dns_cloudflare_records` (`host`, `domain_id`, `cf_record_id`) holds
//! the records it created; a record it holds no id for is never changed
//! or deleted.
//!
//! | Call | What it does |
//! |---|---|
//! | `on-hook("domain_created", {"domain":{"id","host","kind","proxied"}})` | managed only: `GET dns_records?name=<host>`; a record there that is not its own → `deny` ("taken outside liftoff"); else `POST dns_records` (A, `ip`, `ttl` 1, `proxied`) and its id into the table; when storing the id fails the new record is deleted again (v1 7.1 step 5) and the hook denies, so the core takes the domain back |
//! | `on-hook("domain_removed", {"domain":{…}})` | `DELETE dns_records/<id>` of the id it stored for the domain (a record Cloudflare no longer knows counts as deleted), then forgets the id; a domain it holds no id for is left alone |
//! | `handle-route(PATCH /records/{host})` `{"proxied"}` | `PATCH dns_records/<id>` `{"proxied"}`; `200` `{"host","proxied"}`, `404` without a stored record, `502` when Cloudflare fails |
//!
//! The route is how the core's `DomainService::set_proxied` (`lof domain
//! set --proxied`) hands the flag over after its own `authorize`. It is
//! not in the manifest, so `/api/v1/m/dns.cloudflare/...` does not reach
//! it: nobody can flip a record past the core's owner check.

#[expect(
    clippy::error_impl_error,
    clippy::mem_forget,
    clippy::same_length_and_capacity,
    reason = "code wit-bindgen generates"
)]
mod bindings {
    wit_bindgen::generate!({
        path: "../../wit",
        world: "liftoff:module/liftoff-module",
        generate_all,
    });
}

mod cloudflare;
mod http;

use bindings::exports::liftoff::module::module::{Guest, HookResult, Request, Response};
use bindings::liftoff::module::host::{self, Error, Level};
use serde_json::{Value, json};

use crate::cloudflare::{API_HOST, Failure, Zone};

/// The component's export.
struct DnsCloudflare;

/// A dotted-quad IPv4 address, as the config schema checks `ip` at
/// `module configure` (the module parses it again before every use).
const IPV4_PATTERN: &str = r"^((25[0-5]|2[0-4][0-9]|1[0-9]{2}|[1-9]?[0-9])\.){3}(25[0-5]|2[0-4][0-9]|1[0-9]{2}|[1-9]?[0-9])$";

/// The prefix of the route `PATCH /records/{host}`.
const RECORDS_ROUTE: &str = "/records/";

/// A domain of a `domain_*` payload.
#[derive(Debug)]
struct Domain {
    id: String,
    host: String,
    managed: bool,
    proxied: bool,
}

impl Domain {
    /// The `domain` of a hook payload.
    fn of(payload: &str) -> Result<Self, Error> {
        let payload: Value = serde_json::from_str(payload).map_err(|error| Error {
            code: "validation".to_owned(),
            message: format!("hook payload: {error}"),
        })?;
        let domain = payload.get("domain").unwrap_or(&Value::Null);
        let field = |name: &str| {
            domain
                .get(name)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| Error {
                    code: "validation".to_owned(),
                    message: format!("hook payload: domain.{name} is missing"),
                })
        };
        Ok(Self {
            id: field("id")?,
            host: field("host")?,
            managed: domain.get("kind").and_then(Value::as_str) == Some("managed"),
            proxied: domain
                .get("proxied")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
    }
}

fn log(level: Level, message: &str) {
    host::log(level, message);
}

/// The first value of `column` in the rows `store-query` answered.
fn first(rows: &str, column: &str) -> Option<String> {
    let rows: Value = serde_json::from_str(rows).ok()?;
    rows.get(0)?.get(column)?.as_str().map(str::to_owned)
}

/// The id of the record the module created for `host`.
fn record_of_host(host: &str) -> Result<Option<String>, Error> {
    let rows = host::store_query(
        "SELECT cf_record_id FROM m_dns_cloudflare_records WHERE host = ?1",
        &[host.to_owned()],
    )?;
    Ok(first(&rows, "cf_record_id"))
}

/// Keeps `record` as the record of `domain`.
fn keep(domain: &Domain, record: &str) -> Result<(), Error> {
    host::store_query(
        "INSERT INTO m_dns_cloudflare_records (host, domain_id, cf_record_id) VALUES (?1, ?2, ?3) \
         ON CONFLICT (host) DO UPDATE SET domain_id = excluded.domain_id, cf_record_id = excluded.cf_record_id",
        &[domain.host.clone(), domain.id.clone(), record.to_owned()],
    )
    .map(drop)
}

/// `domain_created`: the A record of a managed domain (v1 7.1 steps 3–5).
fn created(domain: &Domain) -> Result<HookResult, Error> {
    if !domain.managed {
        return Ok(HookResult::Continue);
    }
    let zone = match Zone::configured() {
        Ok(zone) => zone,
        Err(why) => {
            log(Level::Error, &why);
            return Ok(HookResult::Deny(why));
        }
    };
    let host = &domain.host;
    let found = match zone.find(host) {
        Ok(found) => found,
        Err(failure) => return Ok(cloudflare_failed(host, &failure)),
    };
    if !found.is_empty() {
        // A record of liftoff's own that outlived its domain (its delete
        // failed): the name is liftoff's, the record is taken over.
        if let Some(own) = record_of_host(host)?.filter(|own| found.contains(own)) {
            keep(domain, &own)?;
            return Ok(HookResult::Continue);
        }
        return Ok(HookResult::Deny(format!(
            "{host} is taken outside liftoff: Cloudflare already has a record of it"
        )));
    }
    let record = match zone.create_a(host, domain.proxied) {
        Ok(record) => record,
        Err(failure) => return Ok(cloudflare_failed(host, &failure)),
    };
    if let Err(error) = keep(domain, &record) {
        // v1 7.1 step 5: a record whose id is not kept would never be
        // deleted again.
        if let Err(failure) = zone.delete(&record) {
            log(
                Level::Error,
                &format!(
                    "could not delete the record {record} of {host} after its id was not stored: {failure}"
                ),
            );
        }
        log(
            Level::Error,
            &format!("could not store the record id of {host}: {}", error.message),
        );
        return Ok(HookResult::Deny(format!(
            "could not keep the DNS record of {host}; it was deleted again"
        )));
    }
    Ok(HookResult::Continue)
}

/// The deny of a hook whose Cloudflare call failed.
///
/// A deny is the only answer a hook has that stops the core, so an
/// outage or a bad token surfaces as the core's `403` with Cloudflare's
/// message (docs/ARCHITECTURE.md, «Модуль `dns.cloudflare`»).
fn cloudflare_failed(host: &str, failure: &Failure) -> HookResult {
    log(Level::Warn, &format!("the DNS record of {host}: {failure}"));
    HookResult::Deny(format!("the DNS record of {host} failed: {failure}"))
}

/// `domain_removed`: deletes the record the module created for the
/// domain, by its stored id only.
fn removed(domain: &Domain) -> Result<HookResult, Error> {
    let rows = host::store_query(
        "SELECT cf_record_id FROM m_dns_cloudflare_records WHERE domain_id = ?1",
        core::slice::from_ref(&domain.id),
    )?;
    let Some(record) = first(&rows, "cf_record_id") else {
        return Ok(HookResult::Continue);
    };
    let zone = match Zone::configured() {
        Ok(zone) => zone,
        Err(why) => {
            log(Level::Error, &why);
            return Ok(HookResult::Deny(why));
        }
    };
    match zone.delete(&record) {
        Ok(()) | Err(Failure::NotFound) => {}
        // The id stays: a later `domain add` of the name takes the
        // record over.
        Err(failure) => return Ok(cloudflare_failed(&domain.host, &failure)),
    }
    host::store_query(
        "DELETE FROM m_dns_cloudflare_records WHERE domain_id = ?1",
        core::slice::from_ref(&domain.id),
    )?;
    Ok(HookResult::Continue)
}

/// A JSON response.
fn respond(status: u16, body: &Value) -> Response {
    Response {
        status,
        headers: vec![("content-type".to_owned(), "application/json".to_owned())],
        body: body.to_string().into_bytes(),
    }
}

/// An `application/problem+json` response, as the core's errors.
fn problem(status: u16, detail: &str) -> Response {
    Response {
        status,
        headers: vec![(
            "content-type".to_owned(),
            "application/problem+json".to_owned(),
        )],
        body: json!({ "status": status, "detail": detail })
            .to_string()
            .into_bytes(),
    }
}

/// `PATCH /records/{host}` `{"proxied"}`.
fn patch_record(host: &str, body: &[u8]) -> Response {
    let Some(proxied) = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|body| body.get("proxied").and_then(Value::as_bool))
    else {
        return problem(400, "the body must be {\"proxied\": true|false}");
    };
    let record = match record_of_host(host) {
        Ok(Some(record)) => record,
        Ok(None) => return problem(404, &format!("no DNS record of {host}")),
        Err(error) => {
            log(
                Level::Error,
                &format!("reading the record of {host}: {}", error.message),
            );
            return problem(500, "the module's records could not be read");
        }
    };
    let zone = match Zone::configured() {
        Ok(zone) => zone,
        Err(why) => return problem(503, &why),
    };
    match zone.set_proxied(&record, proxied) {
        Ok(()) => respond(200, &json!({ "host": host, "proxied": proxied })),
        Err(failure) => {
            log(Level::Warn, &format!("the DNS record of {host}: {failure}"));
            problem(502, &failure.to_string())
        }
    }
}

impl Guest for DnsCloudflare {
    fn manifest() -> String {
        json!({
            "name": "dns.cloudflare",
            "version": env!("CARGO_PKG_VERSION"),
            "config_schema": {
                "type": "object",
                "required": ["zone_id", "api_token", "ip"],
                "additionalProperties": false,
                "properties": {
                    "zone_id": {
                        "type": "string",
                        "description": "Cloudflare zone of the base domain",
                    },
                    "api_token": {
                        "type": "string",
                        "format": "secret",
                        "description": "Cloudflare API token with DNS edit rights on the zone",
                    },
                    "ip": {
                        "type": "string",
                        "pattern": IPV4_PATTERN,
                        "description": "IPv4 address the A records point at (the server's public IP)",
                    },
                },
            },
            "http_allow": [API_HOST],
            "migrations": 1,
            "hooks": ["domain_created", "domain_removed"],
        })
        .to_string()
    }

    fn migrate(from: u32) -> Result<(), Error> {
        if from == 0 {
            host::store_query(
                "CREATE TABLE IF NOT EXISTS m_dns_cloudflare_records (\
                 host TEXT PRIMARY KEY NOT NULL, \
                 domain_id TEXT NOT NULL UNIQUE, \
                 cf_record_id TEXT NOT NULL)",
                &[],
            )?;
        }
        Ok(())
    }

    fn handle_route(req: Request) -> Response {
        if req.actor.is_none() {
            return problem(401, "a session is required");
        }
        match (req.method.as_str(), req.path.strip_prefix(RECORDS_ROUTE)) {
            ("PATCH", Some(host)) if !host.is_empty() && !host.contains(['/', '?']) => {
                patch_record(host, &req.body)
            }
            _ => problem(404, &format!("no route {} {}", req.method, req.path)),
        }
    }

    fn handle_tool(name: String, _args: String) -> Result<String, Error> {
        Err(Error {
            code: "not_found".to_owned(),
            message: format!("tool {name}"),
        })
    }

    fn on_hook(hook: String, payload: String) -> Result<HookResult, Error> {
        match hook.as_str() {
            // A failed notification hook is only logged by the core, and
            // the domain would stay without its record: deny instead.
            "domain_created" => Ok(created(&Domain::of(&payload)?).unwrap_or_else(|error| {
                log(
                    Level::Error,
                    &format!("domain_created failed: {}", error.message),
                );
                HookResult::Deny(format!(
                    "dns.cloudflare could not create the DNS record: {}",
                    error.message
                ))
            })),
            "domain_removed" => removed(&Domain::of(&payload)?),
            _ => Ok(HookResult::Continue),
        }
    }

    fn tick(_name: String) {}
}

/// The exported symbols `wit-bindgen` generates for [`DnsCloudflare`].
#[expect(unsafe_code, reason = "the export glue wit-bindgen generates")]
mod export {
    use super::DnsCloudflare;

    crate::bindings::export!(DnsCloudflare with_types_in crate::bindings);
}
