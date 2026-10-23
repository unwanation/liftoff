//! The provider: discovery, the device authorization grant (RFC 8628),
//! the code exchange and the JWKS, cached in the module's key space.

use serde::{Deserialize, Serialize};

use crate::bindings::liftoff::module::host;
use crate::config::Config;
use crate::http::{self, Answer};
use crate::jwt::{Audience, Claims, Expected, JwkSet, Refusal, Token};

/// RFC 8628 3.4.
const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// The polling interval when the provider names none (RFC 8628 3.2).
const DEFAULT_INTERVAL_SECS: u64 = 5;

/// How long a cached discovery document is used.
const DISCOVERY_TTL_SECS: u64 = 24 * 60 * 60;

/// The key of the cached discovery document.
const DISCOVERY_KEY: &str = "discovery";

/// The key of the cached JWKS (the JWKS cache lives in the
/// module's store, so a reloaded module keeps it).
const JWKS_KEY: &str = "jwks";

/// How long a cached JWKS is trusted: a key the provider took out of its
/// JWKS (rotated away or revoked) stops verifying ID tokens at most this
/// long after, a restart notwithstanding.
const JWKS_TTL_SECS: u64 = 60 * 60;

/// The key of when a `kid` the kept JWKS does not hold last read it
/// again.
const JWKS_REFRESH_KEY: &str = "jwks_refresh";

/// How often a `kid` the kept JWKS does not hold reads it again: a
/// rotated key is accepted within this long, and tokens with forged
/// kids (anyone can send one to `/mcp`) cost at
/// most one fetch of the JWKS per this long.
const JWKS_REFRESH_SECS: u64 = 60;

/// A failure of the provider or of the way to it: `502` for the caller.
#[derive(Debug)]
pub(crate) struct Upstream(pub(crate) String);

/// What a device poll came to.
#[derive(Debug)]
pub(crate) enum Poll {
    /// `authorization_pending` or `slow_down`: ask again.
    Pending,
    /// `access_denied`.
    Denied,
    /// `expired_token`.
    Expired,
    /// The token answer's ID token, not yet checked.
    Done(String),
}

/// The provider's endpoints (`OpenID` Connect Discovery 1.0, 3).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct Discovery {
    issuer: String,
    /// Where the browser login goes.
    pub(crate) authorization_endpoint: String,
    #[serde(default)]
    device_authorization_endpoint: Option<String>,
    token_endpoint: String,
    jwks_uri: String,
}

/// The device authorization answer the CLI gets.
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct DeviceStart {
    device_code: String,
    user_code: String,
    #[serde(default, skip_serializing)]
    verification_uri: Option<String>,
    #[serde(default)]
    verification_uri_complete: Option<String>,
    #[serde(default)]
    interval: Option<u64>,
    expires_in: u64,
}

/// An OAuth error answer (RFC 6749 5.2).
#[derive(Debug, Deserialize)]
struct OAuthError {
    error: String,
}

/// A token answer (RFC 6749 5.1) with the ID token of `OpenID` Connect.
#[derive(Debug, Deserialize)]
struct TokenAnswer {
    #[serde(default)]
    id_token: Option<String>,
}

/// A cached value with its issuer and when it was fetched.
#[derive(Debug, Deserialize, Serialize)]
struct Cached<T> {
    issuer: String,
    fetched_at: u64,
    value: T,
}

fn upstream(why: impl Into<String>) -> Upstream {
    Upstream(why.into())
}

/// Reads `answer` as JSON `T`, a non-2xx answer being the provider's
/// failure.
fn json<T: for<'de> Deserialize<'de>>(answer: &Answer, what: &str) -> Result<T, Upstream> {
    if !answer.is_success() {
        return Err(upstream(format!("{what}: HTTP {}", answer.status)));
    }
    serde_json::from_slice(&answer.body).map_err(|error| upstream(format!("{what}: {error}")))
}

/// The cached value of `key` for `issuer`, if any.
fn cached<T: for<'de> Deserialize<'de>>(key: &str, issuer: &str) -> Option<Cached<T>> {
    host::store_get(key)
        .and_then(|bytes| serde_json::from_slice::<Cached<T>>(&bytes).ok())
        .filter(|cached| cached.issuer == issuer)
}

/// Keeps `value` for `issuer` under `key`.
fn cache<T: Serialize>(key: &str, issuer: &str, now: u64, value: T) {
    let cached = Cached {
        issuer: issuer.to_owned(),
        fetched_at: now,
        value,
    };
    if let Ok(bytes) = serde_json::to_vec(&cached) {
        host::store_set(key, &bytes);
    }
}

/// The provider of `config`.
#[derive(Debug)]
pub(crate) struct Provider<'a> {
    config: &'a Config,
    now: u64,
}

impl<'a> Provider<'a> {
    /// The provider `config` names, at `now` (Unix seconds).
    pub(crate) const fn new(config: &'a Config, now: u64) -> Self {
        Self { config, now }
    }

    /// The discovery document, from the cache while it is fresh.
    pub(crate) fn discovery(&self) -> Result<Discovery, Upstream> {
        let issuer = self.config.issuer.as_str();
        if let Some(cached) = cached::<Discovery>(DISCOVERY_KEY, issuer)
            && cached.fetched_at.saturating_add(DISCOVERY_TTL_SECS) > self.now
        {
            return Ok(cached.value);
        }
        let url = format!(
            "{}/.well-known/openid-configuration",
            issuer.trim_end_matches('/')
        );
        let document: Discovery = json(&http::get(&url).map_err(Upstream)?, "discovery")?;
        // OpenID Connect Discovery 1.0, 4.3: the document is the issuer's.
        if document.issuer != issuer {
            return Err(upstream(format!(
                "discovery names the issuer {:?}",
                document.issuer
            )));
        }
        cache(DISCOVERY_KEY, issuer, self.now, document.clone());
        Ok(document)
    }

    /// Starts a device authorization for the CLI client.
    pub(crate) fn start_device(&self) -> Result<DeviceStart, Upstream> {
        let discovery = self.discovery()?;
        let endpoint = discovery
            .device_authorization_endpoint
            .ok_or_else(|| upstream("the provider has no device authorization endpoint"))?;
        let scope = self.config.scope();
        let answer = http::post_form(
            &endpoint,
            &[("client_id", &self.config.client_id), ("scope", &scope)],
        )
        .map_err(Upstream)?;
        let mut device: DeviceStart = json(&answer, "device authorization")?;
        if device.verification_uri_complete.is_none() {
            device.verification_uri_complete = device.verification_uri.take();
        }
        if device.verification_uri_complete.is_none() {
            return Err(upstream(
                "the device authorization names no verification URI",
            ));
        }
        device.interval = Some(device.interval.unwrap_or(DEFAULT_INTERVAL_SECS));
        Ok(device)
    }

    /// Asks the token endpoint whether `device_code` was confirmed.
    pub(crate) fn poll_device(&self, device_code: &str) -> Result<Poll, Upstream> {
        let discovery = self.discovery()?;
        let answer = http::post_form(
            &discovery.token_endpoint,
            &[
                ("grant_type", DEVICE_GRANT),
                ("device_code", device_code),
                ("client_id", &self.config.client_id),
            ],
        )
        .map_err(Upstream)?;
        if answer.is_success() {
            return id_token(&answer).map(Poll::Done);
        }
        let refused: OAuthError = serde_json::from_slice(&answer.body)
            .map_err(|_unparsed| upstream(format!("token: HTTP {}", answer.status)))?;
        match refused.error.as_str() {
            "authorization_pending" | "slow_down" => Ok(Poll::Pending),
            "access_denied" => Ok(Poll::Denied),
            "expired_token" => Ok(Poll::Expired),
            other => Err(upstream(format!("token: {other}"))),
        }
    }

    /// Trades a browser login's `code` (with its PKCE `verifier`) for an
    /// ID token.
    pub(crate) fn exchange_code(
        &self,
        code: &str,
        verifier: &str,
        redirect_uri: Option<&str>,
    ) -> Result<String, Upstream> {
        let discovery = self.discovery()?;
        let mut fields = vec![
            ("grant_type", "authorization_code"),
            ("code", code),
            ("code_verifier", verifier),
            ("client_id", self.config.client_id.as_str()),
        ];
        if let Some(secret) = &self.config.client_secret {
            fields.push(("client_secret", secret.as_str()));
        }
        if let Some(redirect_uri) = redirect_uri {
            fields.push(("redirect_uri", redirect_uri));
        }
        let answer = http::post_form(&discovery.token_endpoint, &fields).map_err(Upstream)?;
        if !answer.is_success() {
            return Err(upstream(format!("code exchange: HTTP {}", answer.status)));
        }
        id_token(&answer)
    }

    /// Checks `raw` against the provider's keys; a `kid` the cache does
    /// not hold reads the JWKS again once (the provider rotated its keys),
    /// at most once per [`JWKS_REFRESH_SECS`], and a cache older than
    /// [`JWKS_TTL_SECS`] is not used at all.
    /// `Ok(Err(why))` is a token to refuse, `Err` a provider failure.
    pub(crate) fn verify(
        &self,
        raw: &str,
        nonce: Option<&str>,
    ) -> Result<Result<Claims, String>, Upstream> {
        self.verify_for(
            raw,
            &Expected {
                issuer: &self.config.issuer,
                audience: Audience::Client(&self.config.client_id),
                nonce,
                now: self.now,
            },
        )
    }

    /// Checks the access token `raw` of a resource whose tokens carry one
    /// of `audiences` (`/mcp`), as
    /// [`Provider::verify`] does an ID token.
    pub(crate) fn verify_access(
        &self,
        raw: &str,
        audiences: &[String],
    ) -> Result<Result<Claims, String>, Upstream> {
        self.verify_for(
            raw,
            &Expected {
                issuer: &self.config.issuer,
                audience: Audience::AnyOf(audiences),
                nonce: None,
                now: self.now,
            },
        )
    }

    /// Checks `raw` against `expected` (see [`Provider::verify`]).
    fn verify_for(
        &self,
        raw: &str,
        expected: &Expected<'_>,
    ) -> Result<Result<Claims, String>, Upstream> {
        let token = match Token::parse(raw) {
            Ok(token) => token,
            Err(Refusal::Invalid(why)) => return Ok(Err(why)),
            Err(Refusal::UnknownKey) => return Ok(Err("unknown key".to_owned())),
        };
        let issuer = self.config.issuer.as_str();
        let held = cached::<JwkSet>(JWKS_KEY, issuer)
            .filter(|cached| cached.fetched_at.saturating_add(JWKS_TTL_SECS) > self.now)
            .map(|cached| cached.value);
        if let Some(keys) = &held {
            match token.verify(keys, expected) {
                Ok(claims) => return Ok(Ok(claims)),
                Err(Refusal::Invalid(why)) => return Ok(Err(why)),
                Err(Refusal::UnknownKey) => {}
            }
            let refreshed_lately = cached::<()>(JWKS_REFRESH_KEY, issuer)
                .is_some_and(|last| last.fetched_at.saturating_add(JWKS_REFRESH_SECS) > self.now);
            if refreshed_lately {
                return Ok(Err("no key of the JWKS has the token's kid".to_owned()));
            }
            cache(JWKS_REFRESH_KEY, issuer, self.now, ());
        }
        let keys = self.fetch_keys()?;
        Ok(match token.verify(&keys, expected) {
            Ok(claims) => Ok(claims),
            Err(Refusal::Invalid(why)) => Err(why),
            Err(Refusal::UnknownKey) => Err("no key of the JWKS has the token's kid".to_owned()),
        })
    }

    /// Reads the JWKS and keeps it.
    fn fetch_keys(&self) -> Result<JwkSet, Upstream> {
        let discovery = self.discovery()?;
        let keys: JwkSet = json(&http::get(&discovery.jwks_uri).map_err(Upstream)?, "JWKS")?;
        cache(JWKS_KEY, &self.config.issuer, self.now, keys.clone());
        Ok(keys)
    }
}

/// The `id_token` of a token answer.
fn id_token(answer: &Answer) -> Result<String, Upstream> {
    let token: TokenAnswer = json(answer, "token")?;
    token
        .id_token
        .ok_or_else(|| upstream("the token answer has no id_token"))
}
