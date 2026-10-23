//! JSON requests to a Forgejo API through `wasi:http/outgoing-handler`.

use crate::bindings::wasi::http::outgoing_handler;
use crate::bindings::wasi::http::types::{
    Fields, IncomingBody, Method, OutgoingBody, OutgoingRequest, Scheme,
};
use crate::bindings::wasi::io::streams::StreamError;

/// The largest answer the module reads from Forgejo.
const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

/// What `blocking-write-and-flush` takes at once (WASI 0.2).
const WRITE_CHUNK: usize = 4096;

/// What the server answered.
#[derive(Debug)]
pub(crate) struct Answer {
    /// The HTTP status.
    pub(crate) status: u16,
    /// The body.
    pub(crate) body: Vec<u8>,
}

/// One `https` request to `authority` and `path`, with `Authorization:
/// token <token>` (the user's own connection, never `Sudo`) and `body` as
/// JSON; the whole answer (at most [`MAX_BODY_BYTES`]).
///
/// The request is handed to the host before its body is written: the host
/// reads the body only once the request is sent, so a body of more than
/// one [`WRITE_CHUNK`] written first would never be flushed (a CI secret
/// can be long). A body the server stopped reading (it answered early) is
/// not an error: its answer is.
pub(crate) fn send(
    method: &Method,
    authority: &str,
    path: &str,
    token: &str,
    body: Option<&[u8]>,
) -> Result<Answer, String> {
    let mut headers: Vec<(String, Vec<u8>)> = vec![
        ("accept".to_owned(), b"application/json".to_vec()),
        (
            "authorization".to_owned(),
            format!("token {token}").into_bytes(),
        ),
    ];
    if let Some(body) = body {
        headers.push(("content-type".to_owned(), b"application/json".to_vec()));
        headers.push((
            "content-length".to_owned(),
            body.len().to_string().into_bytes(),
        ));
    }
    let fields = Fields::from_list(&headers).map_err(|error| format!("headers: {error:?}"))?;
    let request = OutgoingRequest::new(fields);
    let invalid = |what: &str| format!("the request's {what} was refused");
    request.set_method(method).map_err(|()| invalid("method"))?;
    request
        .set_scheme(Some(&Scheme::Https))
        .map_err(|()| invalid("scheme"))?;
    request
        .set_authority(Some(authority))
        .map_err(|()| invalid("authority"))?;
    request
        .set_path_with_query(Some(path))
        .map_err(|()| invalid("path"))?;
    let outgoing = request.body().map_err(|()| invalid("body"))?;
    let name = method_name(method);
    let pending = outgoing_handler::handle(request, None)
        .map_err(|code| format!("{name} {authority}: {code:?}"))?;
    // Written after `handle` (see above); a failed write leaves the
    // answer to tell what happened.
    if write_body(&outgoing, body.unwrap_or_default()).is_ok() {
        drop(OutgoingBody::finish(outgoing, None));
    }
    pending.subscribe().block();
    let response = pending
        .get()
        .ok_or_else(|| "no response".to_owned())?
        .map_err(|()| "the response was taken twice".to_owned())?
        .map_err(|code| format!("{name} {authority}: {code:?}"))?;
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
        Method::Put => "PUT",
        _ => "request",
    }
}

/// Percent-encodes everything but the unreserved characters (RFC 3986
/// 2.3): one path segment.
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
