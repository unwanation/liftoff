//! The ID token check (v1 `crates/remote/src/oidc.rs`, carried into the
//! module): an RS256 JWS whose signature verifies against a key of the
//! provider's JWKS, issued by the configured issuer for the configured
//! client, not expired. An access token of `/mcp` (RFC 9068) is checked by the same code against the audiences of
//! the resource instead of the client.
//!
//! The signature check is RSASSA-PKCS1-v1_5 with SHA-256 (RFC 8017 8.2.2)
//! written over `num-bigint`: `s^e mod n` is encoded and compared with
//! the expected encoding byte for byte (EMSA-PKCS1-v1_5, step 3 of 8.2.2),
//! never parsed, so no lenient parser can be fooled. Only public values
//! take part, so nothing needs constant time. The `rsa` crate would do the
//! same but carries RUSTSEC-2023-0071 (a timing leak of private-key
//! operations, which a verifier never runs) without a fixed release, and
//! `ring`/`aws-lc-rs` need a C toolchain for `wasm32-wasip2`.

use std::collections::BTreeMap;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use num_bigint::BigUint;
use serde::Deserialize;
use sha2::{Digest as _, Sha256};

/// Zitadel's claim of project roles: `{"<role>": {"<org id>": "<org>"}}`.
pub(crate) const ROLES_CLAIM: &str = "urn:zitadel:iam:org:project:roles";

/// How far the clocks of the provider and of the daemon may differ.
const LEEWAY_SECS: u64 = 60;

/// The smallest RSA modulus accepted (bits).
const MIN_MODULUS_BITS: u64 = 2048;

/// `DigestInfo` of SHA-256 before the hash (RFC 8017 9.2, note 1).
const SHA256_DIGEST_INFO: [u8; 19] = [
    0x30, 0x31, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01, 0x05,
    0x00, 0x04, 0x20,
];

/// A key of the provider's JWKS.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub(crate) struct Jwk {
    kty: String,
    #[serde(default)]
    kid: Option<String>,
    #[serde(default, rename = "use")]
    usage: Option<String>,
    #[serde(default)]
    n: Option<String>,
    #[serde(default)]
    e: Option<String>,
}

/// A JWKS document.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub(crate) struct JwkSet {
    /// Its keys.
    pub(crate) keys: Vec<Jwk>,
}

/// The JOSE header of a token.
#[derive(Debug, Deserialize)]
struct Header {
    alg: String,
    #[serde(default)]
    kid: Option<String>,
}

/// `aud`: one client or several.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Aud {
    One(String),
    Many(Vec<String>),
}

impl Aud {
    fn contains(&self, client: &str) -> bool {
        match self {
            Self::One(one) => one == client,
            Self::Many(many) => many.iter().any(|one| one == client),
        }
    }

    fn contains_any(&self, allowed: &[String]) -> bool {
        allowed.iter().any(|one| self.contains(one))
    }
}

/// The claims the module reads.
#[derive(Debug, Deserialize)]
struct RawClaims {
    iss: String,
    aud: Aud,
    #[serde(default)]
    azp: Option<String>,
    exp: u64,
    #[serde(default)]
    nbf: Option<u64>,
    sub: String,
    #[serde(default)]
    preferred_username: Option<String>,
    #[serde(default)]
    email: String,
    #[serde(default)]
    email_verified: bool,
    #[serde(default)]
    nonce: Option<String>,
    #[serde(default, rename = "urn:zitadel:iam:org:project:roles")]
    roles: BTreeMap<String, serde_json::Value>,
}

/// What a valid ID token says about the person.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Claims {
    /// `sub`.
    pub(crate) subject: String,
    /// `preferred_username`.
    pub(crate) login: String,
    /// `email` when `email_verified`, else empty.
    pub(crate) email: String,
    /// The keys of [`ROLES_CLAIM`].
    pub(crate) roles: Vec<String>,
    /// `exp`, in Unix seconds.
    pub(crate) expires_at: u64,
}

/// Whom a token must be for.
#[derive(Debug)]
pub(crate) enum Audience<'a> {
    /// An ID token of the client: in `aud`, and `azp` when there is one.
    Client(&'a str),
    /// An access token of a resource: one of these in `aud` (`azp` names
    /// the client that asked for it, which may be any).
    AnyOf(&'a [String]),
}

/// What a token is checked against.
#[derive(Debug)]
pub(crate) struct Expected<'a> {
    /// `iss`.
    pub(crate) issuer: &'a str,
    /// Whom the token must be for.
    pub(crate) audience: Audience<'a>,
    /// `nonce`, for a token of the browser login.
    pub(crate) nonce: Option<&'a str>,
    /// Now, in Unix seconds.
    pub(crate) now: u64,
}

/// Why a token is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// No key of the JWKS has the token's `kid`: read the JWKS again.
    UnknownKey,
    /// The token is not valid; the reason is for the log.
    Invalid(String),
}

/// A token split into its parts, its header read.
#[derive(Debug)]
pub(crate) struct Token<'a> {
    header: Header,
    signing_input: &'a str,
    claims: Vec<u8>,
    signature: Vec<u8>,
}

fn invalid(why: impl Into<String>) -> Refusal {
    Refusal::Invalid(why.into())
}

impl<'a> Token<'a> {
    /// Splits `raw` and reads its header; only `RS256` is accepted (so
    /// `alg: none` and HMAC with a public key as secret are refused).
    pub(crate) fn parse(raw: &'a str) -> Result<Self, Refusal> {
        let mut parts = raw.split('.');
        let (Some(header), Some(claims), Some(signature), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(invalid("not a JWS in compact form"));
        };
        let decode = |part: &str, what: &str| {
            URL_SAFE_NO_PAD
                .decode(part)
                .map_err(|error| invalid(format!("{what}: {error}")))
        };
        let header: Header = serde_json::from_slice(&decode(header, "header")?)
            .map_err(|error| invalid(format!("header: {error}")))?;
        if header.alg != "RS256" {
            return Err(invalid(format!("algorithm {:?}", header.alg)));
        }
        let signing_input = raw
            .rsplit_once('.')
            .map(|(input, _)| input)
            .unwrap_or_default();
        Ok(Self {
            header,
            signing_input,
            claims: decode(claims, "claims")?,
            signature: decode(signature, "signature")?,
        })
    }

    /// Verifies the signature against `keys` and checks the claims.
    pub(crate) fn verify(&self, keys: &JwkSet, expected: &Expected<'_>) -> Result<Claims, Refusal> {
        let key = keys
            .keys
            .iter()
            .filter(|key| key.kty == "RSA" && key.usage.as_deref().is_none_or(|u| u == "sig"))
            .find(|key| {
                self.header
                    .kid
                    .as_deref()
                    .is_none_or(|kid| key.kid.as_deref() == Some(kid))
            })
            .ok_or(Refusal::UnknownKey)?;
        let (Some(n), Some(e)) = (&key.n, &key.e) else {
            return Err(invalid("the key has no modulus or exponent"));
        };
        let n = URL_SAFE_NO_PAD
            .decode(n)
            .map_err(|error| invalid(format!("key modulus: {error}")))?;
        let e = URL_SAFE_NO_PAD
            .decode(e)
            .map_err(|error| invalid(format!("key exponent: {error}")))?;
        if !verify_rs256(&n, &e, self.signing_input.as_bytes(), &self.signature) {
            return Err(invalid("the signature does not verify"));
        }
        let claims: RawClaims = serde_json::from_slice(&self.claims)
            .map_err(|error| invalid(format!("claims: {error}")))?;
        check(claims, expected)
    }
}

/// The claim checks of `OpenID` Connect Core 3.1.3.7 the module needs.
fn check(claims: RawClaims, expected: &Expected<'_>) -> Result<Claims, Refusal> {
    if claims.iss != expected.issuer {
        return Err(invalid(format!("issuer {:?}", claims.iss)));
    }
    match expected.audience {
        Audience::Client(client) => {
            if !claims.aud.contains(client) {
                return Err(invalid("the audience is not this client"));
            }
            if claims.azp.as_deref().is_some_and(|azp| azp != client) {
                return Err(invalid("authorized for another client"));
            }
        }
        Audience::AnyOf(allowed) => {
            if !claims.aud.contains_any(allowed) {
                return Err(invalid("the audience is not allowed"));
            }
        }
    }
    if claims.exp.saturating_add(LEEWAY_SECS) <= expected.now {
        return Err(invalid("expired"));
    }
    if claims
        .nbf
        .is_some_and(|nbf| nbf > expected.now.saturating_add(LEEWAY_SECS))
    {
        return Err(invalid("not valid yet"));
    }
    if let Some(nonce) = expected.nonce
        && claims.nonce.as_deref() != Some(nonce)
    {
        return Err(invalid("the nonce does not match"));
    }
    let login = claims
        .preferred_username
        .filter(|login| !login.is_empty())
        .ok_or_else(|| invalid("no preferred_username"))?;
    if claims.sub.is_empty() {
        return Err(invalid("no subject"));
    }
    Ok(Claims {
        subject: claims.sub,
        login,
        email: if claims.email_verified {
            claims.email
        } else {
            String::new()
        },
        roles: claims.roles.into_keys().collect(),
        expires_at: claims.exp,
    })
}

/// RSASSA-PKCS1-v1_5 verification with SHA-256 of `message` under the
/// public key `(n, e)` (big-endian bytes), RFC 8017 8.2.2.
fn verify_rs256(modulus: &[u8], exponent: &[u8], message: &[u8], signature: &[u8]) -> bool {
    let modulus = BigUint::from_bytes_be(modulus);
    let exponent = BigUint::from_bytes_be(exponent);
    let bits = modulus.bits();
    let Ok(length) = usize::try_from(bits.div_ceil(8)) else {
        return false;
    };
    // An even or tiny exponent is no RSA key; a short modulus is too weak.
    let three = BigUint::from(3_u8);
    if bits < MIN_MODULUS_BITS || exponent < three || !exponent.bit(0) || signature.len() != length
    {
        return false;
    }
    let signed = BigUint::from_bytes_be(signature);
    if signed >= modulus {
        return false;
    }
    let recovered = signed.modpow(&exponent, &modulus).to_bytes_be();
    let Some(padding) = length.checked_sub(recovered.len()) else {
        return false;
    };
    let mut encoded = vec![0_u8; padding];
    encoded.extend_from_slice(&recovered);
    expected_encoding(length, &Sha256::digest(message)).is_some_and(|expected| encoded == expected)
}

/// EMSA-PKCS1-v1_5 of the SHA-256 `digest` in `k` bytes:
/// `00 01 FF…FF 00 DigestInfo digest`.
fn expected_encoding(k: usize, digest: &[u8]) -> Option<Vec<u8>> {
    let t_len = SHA256_DIGEST_INFO.len().checked_add(digest.len())?;
    let filler = k.checked_sub(t_len)?.checked_sub(3)?;
    if filler < 8 {
        return None;
    }
    let mut encoded = Vec::with_capacity(k);
    encoded.extend_from_slice(&[0x00, 0x01]);
    encoded.resize(filler.checked_add(2)?, 0xff);
    encoded.push(0x00);
    encoded.extend_from_slice(&SHA256_DIGEST_INFO);
    encoded.extend_from_slice(digest);
    Some(encoded)
}
