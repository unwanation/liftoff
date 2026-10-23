//! `fn_rust`: the contract function in Rust (FNC-01…06), of the world `liftoff:function/function`
//! (`wit/function/`). The TS and Python contract functions of
//! answer exactly the same.
//!
//! | Request | Answer |
//! |---|---|
//! | `GET /` | `200`, `application/json`: `{"method", "path", "greeting", "counter", "fetch"}` |
//! | `POST /echo` | `200` with the request body and its `content-type` |
//! | anything else | `404` |
//!
//! - `method`, `path`: the request's (`path` with its query);
//! - `greeting`: the `wasi:config` value `GREETING`, `null` without one;
//! - `counter`: `wasi:keyvalue/atomics.increment` of the key `counter` in
//!   the bucket `default` by 1, the new value (`null` on a `wasi:keyvalue`
//!   error);
//! - `fetch`: with the `wasi:config` value `FETCH_URL`
//!   (`http://host:port/path`), the body of a `GET` there through
//!   `wasi:http/outgoing-handler` as a string, or `"error: <code>"`;
//!   `null` without one.
//!
//! The daemon's warm-up (`GET /` with `X-Liftoff-Warmup`) is
//! no visitor: it answers the same JSON with `counter` and `fetch`
//! `null`, so it neither counts nor reaches out.

#[expect(
    clippy::error_impl_error,
    clippy::same_length_and_capacity,
    reason = "code wit-bindgen generates"
)]
mod bindings {
    wit_bindgen::generate!({
        path: "../../../../wit/function",
        world: "liftoff:function/function",
        generate_all,
    });
}

use bindings::exports::wasi::http::incoming_handler::Guest;
use bindings::wasi::config::store as config;
use bindings::wasi::http::outgoing_handler;
use bindings::wasi::http::types::{
    ErrorCode, Fields, IncomingBody, IncomingRequest, InputStream, Method, OutgoingBody,
    OutgoingRequest, OutgoingResponse, ResponseOutparam, Scheme,
};
use bindings::wasi::io::streams::StreamError;
use bindings::wasi::keyvalue::{atomics, store};
use serde_json::{Value, json};

/// The component's export.
struct Contract;

impl Guest for Contract {
    fn handle(request: IncomingRequest, response_out: ResponseOutparam) {
        let target = request.path_with_query().unwrap_or_default();
        let path = target
            .split_once('?')
            .map_or(target.as_str(), |(path, _)| path);
        match (request.method(), path) {
            (Method::Get, "/") => {
                let visitor = request.headers().get("x-liftoff-warmup").is_empty();
                let answer = json!({
                    "method": "GET",
                    "path": target,
                    "greeting": config::get("GREETING").ok().flatten(),
                    "counter": if visitor { counter() } else { Value::Null },
                    "fetch": config::get("FETCH_URL")
                        .ok()
                        .flatten()
                        .filter(|_| visitor)
                        .map(|url| fetch(&url)),
                });
                respond(
                    response_out,
                    200,
                    "application/json",
                    answer.to_string().as_bytes(),
                );
            }
            (Method::Post, "/echo") => {
                let content_type = request
                    .headers()
                    .get("content-type")
                    .into_iter()
                    .next()
                    .map_or_else(
                        || "application/octet-stream".to_owned(),
                        |raw| String::from_utf8_lossy(&raw).into_owned(),
                    );
                let body = request_body(&request);
                respond(response_out, 200, &content_type, &body);
            }
            _ => respond(response_out, 404, "text/plain", b"not found"),
        }
    }
}

/// The counter after one more request, or `null`.
fn counter() -> Value {
    store::open("default")
        .and_then(|bucket| atomics::increment(&bucket, "counter", 1))
        .map_or(Value::Null, Value::from)
}

/// Reads `stream` to its end.
fn read_all(stream: &InputStream) -> Vec<u8> {
    let mut read = Vec::new();
    loop {
        match stream.blocking_read(64 * 1024) {
            Ok(chunk) => read.extend(chunk),
            Err(StreamError::Closed | StreamError::LastOperationFailed(_)) => return read,
        }
    }
}

/// The body of `request`.
fn request_body(request: &IncomingRequest) -> Vec<u8> {
    let Ok(body) = request.consume() else {
        return Vec::new();
    };
    let read = body
        .stream()
        .map(|stream| read_all(&stream))
        .unwrap_or_default();
    let _trailers = IncomingBody::finish(body);
    read
}

/// The body of `GET url` as a string, or `"error: <code>"`.
fn fetch(url: &str) -> String {
    match get(url) {
        Ok(body) => String::from_utf8_lossy(&body).into_owned(),
        Err(code) => {
            // wit-bindgen's `Debug` spells the case `ErrorCode::Name`; the
            // contract answers the bare case name, as the TS and Python
            // functions do.
            let spelled = format!("{code:?}");
            let name = spelled.strip_prefix("ErrorCode::").unwrap_or(&spelled);
            format!("error: {name}")
        }
    }
}

/// `GET url` (`http://authority/path`) through the host.
fn get(url: &str) -> Result<Vec<u8>, ErrorCode> {
    let refused = |what: &str| ErrorCode::InternalError(Some(format!("bad {what}")));
    let (scheme, rest) = url.split_once("://").ok_or_else(|| refused("url"))?;
    let (authority, path) = rest.find('/').map_or((rest, "/"), |at| rest.split_at(at));
    let scheme = if scheme == "https" {
        Scheme::Https
    } else {
        Scheme::Http
    };
    let request = OutgoingRequest::new(Fields::new());
    request
        .set_scheme(Some(&scheme))
        .map_err(|()| refused("scheme"))?;
    request
        .set_authority(Some(authority))
        .map_err(|()| refused("authority"))?;
    request
        .set_path_with_query(Some(path))
        .map_err(|()| refused("path"))?;
    let pending = outgoing_handler::handle(request, None)?;
    pending.subscribe().block();
    let response = pending
        .get()
        .ok_or_else(|| refused("pending response"))?
        .map_err(|()| refused("taken response"))??;
    let body = response.consume().map_err(|()| refused("taken body"))?;
    let read = body
        .stream()
        .map(|stream| read_all(&stream))
        .unwrap_or_default();
    let _trailers = IncomingBody::finish(body);
    Ok(read)
}

/// Sends the response.
fn respond(response_out: ResponseOutparam, status: u16, content_type: &str, body: &[u8]) {
    let headers =
        Fields::from_list(&[("content-type".to_owned(), content_type.as_bytes().to_vec())])
            .unwrap_or_else(|_| Fields::new());
    let response = OutgoingResponse::new(headers);
    let _status: Result<(), ()> = response.set_status_code(status);
    let Ok(outgoing) = response.body() else {
        return;
    };
    ResponseOutparam::set(response_out, Ok(response));
    if let Ok(stream) = outgoing.write() {
        for chunk in body.chunks(4096) {
            if stream.blocking_write_and_flush(chunk).is_err() {
                break;
            }
        }
        drop(stream);
    }
    let _finished: Result<(), ErrorCode> = OutgoingBody::finish(outgoing, None);
}

/// The exported symbols `wit-bindgen` generates for [`Contract`].
#[expect(unsafe_code, reason = "the export glue wit-bindgen generates")]
mod export {
    use super::Contract;

    crate::bindings::export!(Contract with_types_in crate::bindings);
}
