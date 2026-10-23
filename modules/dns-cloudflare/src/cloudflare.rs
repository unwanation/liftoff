//! The Cloudflare v4 DNS records API of one zone (v1's
//! `crates/remote/src/cloudflare.rs`, over `wasi:http`).

use serde_json::{Value, json};

use crate::bindings::liftoff::module::host;
use crate::bindings::wasi::http::types::Method;
use crate::http::{self, encode};

/// The API's host, the only one in the module's `http_allow`.
pub(crate) const API_HOST: &str = "api.cloudflare.com";

/// Why a call failed.
#[derive(Debug)]
pub(crate) enum Failure {
    /// `404`: the record does not exist (any more).
    NotFound,
    /// Anything else: the API's first error message, or what went wrong.
    Other(String),
}

impl core::fmt::Display for Failure {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotFound => f.write_str("Cloudflare does not know the record"),
            Self::Other(why) => write!(f, "Cloudflare: {why}"),
        }
    }
}

/// The DNS records of the configured zone, reached with the configured
/// token.
#[derive(Debug)]
pub(crate) struct Zone {
    /// The zone's id.
    id: String,
    token: String,
    /// The IPv4 address A records point at.
    ip: String,
}

impl Zone {
    /// The zone of the module's configuration (`zone_id`, `api_token`,
    /// `ip`); what is missing or wrong when it is incomplete or `ip` is
    /// not an IPv4 address.
    pub(crate) fn configured() -> Result<Self, String> {
        let get = |key: &str| {
            host::config_get(key)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| format!("dns.cloudflare is not configured: {key} is missing"))
        };
        let ip = get("ip")?;
        let ip = ip
            .parse::<core::net::Ipv4Addr>()
            .map_err(|_invalid| {
                "dns.cloudflare is misconfigured: ip is not an IPv4 address".to_owned()
            })?
            .to_string();
        Ok(Self {
            id: get("zone_id")?,
            token: get("api_token")?,
            ip,
        })
    }

    /// The ids of the records named `name`.
    pub(crate) fn find(&self, name: &str) -> Result<Vec<String>, Failure> {
        let path = format!("{}?name={}", self.records(), encode(name));
        let found = self.call(&Method::Get, &path, None)?;
        Ok(found
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|record| record.get("id").and_then(Value::as_str).map(str::to_owned))
            .collect())
    }

    /// Creates the A record `name` → the configured IP with automatic TTL;
    /// its id.
    pub(crate) fn create_a(&self, name: &str, proxied: bool) -> Result<String, Failure> {
        let body = json!({
            "type": "A",
            "name": name,
            "content": self.ip,
            "ttl": 1,
            "proxied": proxied,
        });
        let created = self.call(&Method::Post, &self.records(), Some(&body))?;
        created
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| Failure::Other("the created record has no id".to_owned()))
    }

    /// Sets whether Cloudflare proxies the record `id`.
    pub(crate) fn set_proxied(&self, id: &str, proxied: bool) -> Result<(), Failure> {
        let body = json!({ "proxied": proxied });
        self.call(&Method::Patch, &self.record(id), Some(&body))
            .map(drop)
    }

    /// Deletes the record `id`.
    pub(crate) fn delete(&self, id: &str) -> Result<(), Failure> {
        self.call(&Method::Delete, &self.record(id), None).map(drop)
    }

    /// The path of the zone's records.
    fn records(&self) -> String {
        format!("/client/v4/zones/{}/dns_records", encode(&self.id))
    }

    /// The path of the record `id`.
    fn record(&self, id: &str) -> String {
        format!("{}/{}", self.records(), encode(id))
    }

    /// Sends one request and unwraps Cloudflare's `{success, errors,
    /// result}` envelope: its `result`.
    fn call(&self, method: &Method, path: &str, body: Option<&Value>) -> Result<Value, Failure> {
        let body = body.map(|body| body.to_string().into_bytes());
        let answer = http::send(method, API_HOST, path, &self.token, body.as_deref())
            .map_err(Failure::Other)?;
        if answer.status == 404 {
            return Err(Failure::NotFound);
        }
        let success = (200..300).contains(&answer.status);
        match serde_json::from_slice::<Value>(&answer.body) {
            Ok(envelope)
                if success && envelope.get("success").and_then(Value::as_bool) == Some(true) =>
            {
                Ok(envelope.get("result").cloned().unwrap_or(Value::Null))
            }
            Ok(envelope) => Err(Failure::Other(
                envelope
                    .pointer("/errors/0/message")
                    .and_then(Value::as_str)
                    .map_or_else(|| format!("HTTP {}", answer.status), str::to_owned),
            )),
            Err(_malformed) if success => Err(Failure::Other("malformed response".to_owned())),
            Err(_malformed) => Err(Failure::Other(format!("HTTP {}", answer.status))),
        }
    }
}
