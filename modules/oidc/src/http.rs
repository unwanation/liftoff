//! Requests to the provider through `wasi:http/outgoing-handler`, and the
//! bits of URLs and forms they need.

use crate::bindings::wasi::http::outgoing_handler;
use crate::bindings::wasi::http::types::{
    Fields, IncomingBody, Method, OutgoingBody, OutgoingRequest, Scheme,
};
use crate::bindings::wasi::io::streams::StreamError;

/// The largest answer the module reads from the provider.
const MAX_BODY_BYTES: usize = 1024 * 1024;

/// What `blocking-write-and-flush` takes at once (WASI 0.2).
const WRITE_CHUNK: usize = 4096;

/// The largest request body the module sends: one [`WRITE_CHUNK`], what
/// the host's outgoing body buffers before the request is handed over
/// (`wasmtime-wasi-http` keeps one write and a free slot, and
/// `blocking-write-and-flush` waits for that slot), so the whole body is
/// written before `handle` and a provider that answers without reading
/// it cannot close the stream under the module.
const MAX_REQUEST_BODY_BYTES: usize = WRITE_CHUNK;

/// What the provider answered.
#[derive(Debug)]
pub(crate) struct Answer {
    /// The HTTP status.
    pub(crate) status: u16,
    /// The body.
    pub(crate) body: Vec<u8>,
}

impl Answer {
    /// Whether the status is `2xx`.
    pub(crate) const fn is_success(&self) -> bool {
        self.status >= 200 && self.status < 300
    }
}

/// An absolute `http(s)` URL split the way `wasi:http` wants it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Url {
    /// `true` for `https`.
    pub(crate) secure: bool,
    /// `host[:port]`.
    pub(crate) authority: String,
    /// The path with the query, `/` at least.
    pub(crate) path: String,
}

impl Url {
    /// Parses `http://` or `https://` URLs without user info.
    pub(crate) fn parse(raw: &str) -> Result<Self, String> {
        let invalid = || format!("not an http(s) URL: {raw:?}");
        let (secure, rest) = if let Some(rest) = raw.strip_prefix("https://") {
            (true, rest)
        } else if let Some(rest) = raw.strip_prefix("http://") {
            (false, rest)
        } else {
            return Err(invalid());
        };
        let cut = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let (authority, path) = rest.split_at(cut);
        let path = path.split('#').next().unwrap_or_default();
        if authority.is_empty() || authority.contains('@') {
            return Err(invalid());
        }
        let path = if path.is_empty() {
            "/".to_owned()
        } else if path.starts_with('?') {
            format!("/{path}")
        } else {
            path.to_owned()
        };
        Ok(Self {
            secure,
            authority: authority.to_owned(),
            path,
        })
    }
}

/// `GET url`.
pub(crate) fn get(url: &str) -> Result<Answer, String> {
    send(&Method::Get, url, None)
}

/// `POST url` with `fields` as `application/x-www-form-urlencoded`.
pub(crate) fn post_form(url: &str, fields: &[(&str, &str)]) -> Result<Answer, String> {
    send(&Method::Post, url, Some(form(fields).as_bytes()))
}

/// `fields` as a form (or query) string.
pub(crate) fn form(fields: &[(&str, &str)]) -> String {
    fields
        .iter()
        .map(|(key, value)| format!("{}={}", encode(key), encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Percent-encodes everything but the unreserved characters (RFC 3986
/// 2.3).
pub(crate) fn encode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push('%');
            out.push(hex_digit(byte >> 4));
            out.push(hex_digit(byte & 0x0f));
        }
    }
    out
}

/// The upper-case hex digit of `nibble` (`0..16`).
fn hex_digit(nibble: u8) -> char {
    char::from_digit(u32::from(nibble), 16).map_or('0', |digit| digit.to_ascii_uppercase())
}

/// The value of `name` in the query string `query` (`a=1&b=2`), decoded.
pub(crate) fn query_value(query: &str, name: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        (decode(key) == name).then(|| decode(value))
    })
}

/// Decodes `%XX` and `+` of a form value; a broken escape stays as it is.
fn decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut rest = bytes;
    while let Some((&first, tail)) = rest.split_first() {
        match first {
            b'+' => {
                out.push(b' ');
                rest = tail;
            }
            b'%' => {
                let escaped = tail
                    .get(..2)
                    .and_then(|hex| core::str::from_utf8(hex).ok())
                    .and_then(|hex| u8::from_str_radix(hex, 16).ok());
                if let Some(byte) = escaped {
                    out.push(byte);
                    rest = tail.get(2..).unwrap_or_default();
                } else {
                    out.push(first);
                    rest = tail;
                }
            }
            _ => {
                out.push(first);
                rest = tail;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Sends one request and reads the whole answer (at most
/// [`MAX_BODY_BYTES`]).
///
/// The body (at most [`MAX_REQUEST_BODY_BYTES`], with its
/// `content-length`) is written and finished before the request is
/// handed to `outgoing-handler`: written after, it races the answer of a
/// server that replies without reading it, and loses with
/// `StreamError::Closed` when the answer came first.
fn send(method: &Method, url: &str, form_body: Option<&[u8]>) -> Result<Answer, String> {
    let url = Url::parse(url)?;
    let mut headers: Vec<(String, Vec<u8>)> =
        vec![("accept".to_owned(), b"application/json".to_vec())];
    if let Some(body) = form_body {
        if body.len() > MAX_REQUEST_BODY_BYTES {
            return Err(format!(
                "the request body is over {MAX_REQUEST_BODY_BYTES} bytes"
            ));
        }
        headers.push((
            "content-type".to_owned(),
            b"application/x-www-form-urlencoded".to_vec(),
        ));
        headers.push((
            "content-length".to_owned(),
            body.len().to_string().into_bytes(),
        ));
    }
    let fields = Fields::from_list(&headers).map_err(|error| format!("headers: {error:?}"))?;
    let request = OutgoingRequest::new(fields);
    let invalid = |what: &str| format!("the request's {what} was refused");
    request.set_method(method).map_err(|()| invalid("method"))?;
    let scheme = if url.secure {
        Scheme::Https
    } else {
        Scheme::Http
    };
    request
        .set_scheme(Some(&scheme))
        .map_err(|()| invalid("scheme"))?;
    request
        .set_authority(Some(&url.authority))
        .map_err(|()| invalid("authority"))?;
    request
        .set_path_with_query(Some(&url.path))
        .map_err(|()| invalid("path"))?;
    let outgoing = request.body().map_err(|()| invalid("body"))?;
    write_body(&outgoing, form_body.unwrap_or_default())?;
    OutgoingBody::finish(outgoing, None).map_err(|code| format!("sending the body: {code:?}"))?;
    let pending = outgoing_handler::handle(request, None)
        .map_err(|code| format!("{} {url:?}: {code:?}", method_name(method)))?;
    pending.subscribe().block();
    let response = pending
        .get()
        .ok_or_else(|| "no response".to_owned())?
        .map_err(|()| "the response was taken twice".to_owned())?
        .map_err(|code| format!("{} {}: {code:?}", method_name(method), url.authority))?;
    let status = response.status();
    let incoming = response
        .consume()
        .map_err(|()| "the response body was taken twice".to_owned())?;
    let body = read_body(&incoming)?;
    drop(IncomingBody::finish(incoming));
    Ok(Answer { status, body })
}

const fn method_name(method: &Method) -> &'static str {
    match method {
        Method::Get => "GET",
        Method::Post => "POST",
        _ => "request",
    }
}

/// Writes `bytes` to the request body.
fn write_body(body: &OutgoingBody, bytes: &[u8]) -> Result<(), String> {
    let stream = body
        .write()
        .map_err(|()| "the request body was taken twice".to_owned())?;
    for chunk in bytes.chunks(WRITE_CHUNK) {
        stream
            .blocking_write_and_flush(chunk)
            .map_err(|error| format!("writing the body: {error:?}"))?;
    }
    drop(stream);
    Ok(())
}

/// Reads the response body to its end.
fn read_body(body: &IncomingBody) -> Result<Vec<u8>, String> {
    let stream = body
        .stream()
        .map_err(|()| "the response stream was taken twice".to_owned())?;
    let mut read = Vec::new();
    loop {
        match stream.blocking_read(64 * 1024) {
            Ok(chunk) => {
                read.extend_from_slice(&chunk);
                if read.len() > MAX_BODY_BYTES {
                    return Err(format!("the answer is over {MAX_BODY_BYTES} bytes"));
                }
            }
            Err(StreamError::Closed) => break,
            Err(StreamError::LastOperationFailed(error)) => {
                return Err(format!("reading the answer: {}", error.to_debug_string()));
            }
        }
    }
    drop(stream);
    Ok(read)
}
