//! The function of {{project}}.
//!
//! - `GET /`: `200`, JSON `{"greeting", "visits"}`: the secret `GREETING`
//!   (read through `wasi:config`) or a default, and how many times the
//!   page was asked for (a counter in `wasi:keyvalue`);
//! - `POST /echo`: `200` with the request's body and `content-type`;
//! - anything else: `404`.

mod bindings {
    wit_bindgen::generate!({
        path: "wit",
        world: "liftoff:function/function",
        generate_all,
    });
}

use bindings::exports::wasi::http::incoming_handler::Guest;
use bindings::wasi::config::store as config;
use bindings::wasi::http::types::{
    ErrorCode, Fields, IncomingBody, IncomingRequest, Method, OutgoingBody, OutgoingResponse,
    ResponseOutparam,
};
use bindings::wasi::keyvalue::{atomics, store};
use serde_json::json;

struct Function;

impl Guest for Function {
    fn handle(request: IncomingRequest, response_out: ResponseOutparam) {
        let target = request.path_with_query().unwrap_or_default();
        let path = target.split('?').next().unwrap_or_default();
        match (request.method(), path) {
            (Method::Get, "/") => {
                // liftoff warms a new deploy up with one `GET /` that
                // carries this header: it is no visit.
                let visit = request.headers().get("x-liftoff-warmup").is_empty();
                let greeting = config::get("GREETING")
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| "Hello from {{project}}".to_owned());
                let visits = if visit { visits() } else { None };
                let answer = json!({ "greeting": greeting, "visits": visits });
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
            _ => respond(response_out, 404, "text/plain", b"not found\n"),
        }
    }
}

/// The counter `visits` after one more visit, or `None` when the
/// key-value store refuses. The bucket `""` is the environment's one on
/// liftoff and the in-memory one of `wasmtime serve`.
fn visits() -> Option<u64> {
    store::open("")
        .and_then(|bucket| atomics::increment(&bucket, "visits", 1))
        .ok()
}

/// The body of `request`.
fn request_body(request: &IncomingRequest) -> Vec<u8> {
    let mut read = Vec::new();
    let Ok(body) = request.consume() else {
        return read;
    };
    if let Ok(stream) = body.stream() {
        // The stream ends with `Closed` (or the error of a failed read).
        while let Ok(chunk) = stream.blocking_read(64 * 1024) {
            read.extend(chunk);
        }
    }
    let _trailers = IncomingBody::finish(body);
    read
}

/// Sends the response.
fn respond(response_out: ResponseOutparam, status: u16, content_type: &str, body: &[u8]) {
    let headers =
        Fields::from_list(&[("content-type".to_owned(), content_type.as_bytes().to_vec())])
            .unwrap_or_else(|_| Fields::new());
    let response = OutgoingResponse::new(headers);
    let _ = response.set_status_code(status);
    let Ok(outgoing) = response.body() else {
        return;
    };
    ResponseOutparam::set(response_out, Ok(response));
    if let Ok(stream) = outgoing.write() {
        // One write takes at most 4096 bytes.
        for chunk in body.chunks(4096) {
            if stream.blocking_write_and_flush(chunk).is_err() {
                break;
            }
        }
    }
    let _: Result<(), ErrorCode> = OutgoingBody::finish(outgoing, None);
}

bindings::export!(Function with_types_in bindings);
