//! `fn_misbehave`: a contract component of the function runtime (FN-03…FN-10), of the world
//! `liftoff:function/function` (`wit/function/`).
//!
//! | Request | What it does |
//! |---|---|
//! | `GET /` | `200 ok`; with `X-Liftoff-Warmup` it first sets the key `warmup` to `"<method> <path>"`, and then traps when the config `WARMUP` is `trap`, answers `500` when it is `fail` or runs forever when it is `spin` |
//! | `GET /instance` | `200` with how many requests this instance has served, this one included |
//! | `GET /panic` | panics: the request traps |
//! | `GET /grow?bytes=N` | allocates and fills `N` bytes, then `200 ok` |
//! | `GET /spin` | runs forever in wasm code |
//! | `GET /silent` | returns without setting a response |
//! | `GET /big?bytes=N` | `200` with a body of `N` zero bytes, written 4 KiB at a time |
//! | `GET /files?path=P` | reads the file `P` through `std::fs`: `200` with its bytes, `403` with the error |
//! | `GET /sleep?ms=N` | waits `N` ms on the host's clock, then `200 ok` |
//! | `GET /fetch?url=http://host:port/path` | sends `GET` there through `wasi:http/outgoing-handler`; answers the upstream status and body, or `502` with `fetch error: <code>` |
//! | `GET /print?text=T` | writes `out T` to stdout and `err T` to stderr, then `200 ok` |
//! | `GET /config?key=K` | `200` with the `wasi:config` value of `K`, `404` when there is none |
//! | `GET /kv/open?name=N` | opens the bucket `N`: `200 ok` |
//! | `GET /kv/get?key=K` | `200` with the value of `K` in the bucket `default`, `404` when there is none |
//! | `POST /kv/set?key=K` | sets `K` to the body, `204` |
//! | `POST /kv/delete?key=K` | deletes `K`, `204` |
//! | `GET /kv/incr?key=K` | `atomics.increment` by 1, `200` with the new value |
//! | `POST /kv/set-many` | `batch.set-many` of the body's `k=v` pairs (joined by `&`), `204` |
//! | `GET /kv/get-many?keys=a,b` | `batch.get-many`: the values joined by `,`, `-` for a missing one |
//! | anything else | `404` |
//!
//! A `wasi:keyvalue` error answers `507` with the error: `other(<message>)`
//! as the message, else the variant's name.

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

use core::sync::atomic::{AtomicU64, Ordering};
use std::io::Write as _;

use bindings::exports::wasi::http::incoming_handler::Guest;
use bindings::wasi::clocks::monotonic_clock;
use bindings::wasi::config::store as config;
use bindings::wasi::http::outgoing_handler;
use bindings::wasi::http::types::{
    ErrorCode, Fields, IncomingBody, IncomingRequest, InputStream, Method, OutgoingBody,
    OutgoingRequest, OutgoingResponse, ResponseOutparam, Scheme,
};
use bindings::wasi::io::streams::StreamError;
use bindings::wasi::keyvalue::{atomics, batch, store};

/// Requests this instance has served.
static SERVED: AtomicU64 = AtomicU64::new(0);

/// The component's export.
struct Misbehave;

/// What a request answers.
struct Answer {
    status: u16,
    body: Vec<u8>,
}

impl Answer {
    fn new(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            body: body.into(),
        }
    }

    fn ok() -> Self {
        Self::new(200, "ok")
    }

    fn not_found() -> Self {
        Self::new(404, "not found")
    }
}

impl Guest for Misbehave {
    fn handle(request: IncomingRequest, response_out: ResponseOutparam) {
        let served = SERVED.fetch_add(1, Ordering::SeqCst).saturating_add(1);
        match route(&request, served) {
            Reply::Whole(answer) => respond(response_out, &answer),
            Reply::Zeros(bytes) => stream_zeros(response_out, bytes),
            // The outparam dropped unset: the request has no response.
            Reply::Silent => drop(response_out),
        }
    }
}

/// The value of `name` in `query` (`a=b&c=d`), undecoded.
fn param<'q>(query: &'q str, name: &str) -> Option<&'q str> {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value)
}

/// What a request answers, if anything.
enum Reply {
    /// A body held in memory.
    Whole(Answer),
    /// `N` zero bytes, written in chunks.
    Zeros(u64),
    /// No response at all.
    Silent,
}

fn route(request: &IncomingRequest, served: u64) -> Reply {
    let target = request.path_with_query().unwrap_or_default();
    let (path, query) = target.split_once('?').unwrap_or((target.as_str(), ""));
    match (request.method(), path) {
        (Method::Get, "/silent") => Reply::Silent,
        (Method::Get, "/big") => param(query, "bytes")
            .and_then(|raw| raw.parse::<u64>().ok())
            .map_or_else(
                || Reply::Whole(Answer::new(400, "bytes missing")),
                Reply::Zeros,
            ),
        _ => Reply::Whole(answer(request, served)),
    }
}

fn answer(request: &IncomingRequest, served: u64) -> Answer {
    let target = request.path_with_query().unwrap_or_default();
    let (path, query) = target.split_once('?').unwrap_or((target.as_str(), ""));
    let method = request.method();
    match (&method, path) {
        (Method::Get, "/") => root(request),
        (Method::Get, "/instance") => Answer::new(200, served.to_string()),
        (Method::Get, "/panic") => {
            #[expect(clippy::panic, reason = "the path exists to trap")]
            {
                panic!("fn_misbehave traps on request")
            }
        }
        (Method::Get, "/grow") => grow(query),
        (Method::Get, "/spin") => spin(),
        (Method::Get, "/files") => files(query),
        (Method::Get, "/sleep") => sleep(query),
        (Method::Get, "/fetch") => param(query, "url").map_or_else(
            || Answer::new(400, "url missing"),
            |url| {
                fetch(url).unwrap_or_else(|code| Answer::new(502, format!("fetch error: {code:?}")))
            },
        ),
        (Method::Get, "/print") => {
            let text = param(query, "text").unwrap_or_default();
            // An explicit write to each stream, as the runtime sees it.
            let out = writeln!(std::io::stdout(), "out {text}");
            let err = writeln!(std::io::stderr(), "err {text}");
            if out.is_err() || err.is_err() {
                return Answer::new(500, "could not write");
            }
            Answer::ok()
        }
        (Method::Get, "/config") => {
            param(query, "key").map_or_else(Answer::not_found, |key| match config::get(key) {
                Ok(Some(value)) => Answer::new(200, value),
                Ok(None) => Answer::not_found(),
                Err(error) => Answer::new(500, format!("config error: {error:?}")),
            })
        }
        (_, kv) if kv.starts_with("/kv/") => {
            keyvalue(request, &method, kv, query).unwrap_or_else(|error| Answer::new(507, error))
        }
        _ => Answer::not_found(),
    }
}

/// `GET /`, and the warm-up.
fn root(request: &IncomingRequest) -> Answer {
    let warmup = !request.headers().get("x-liftoff-warmup").is_empty();
    if !warmup {
        return Answer::ok();
    }
    let seen = format!("GET {}", request.path_with_query().unwrap_or_default());
    if let Err(error) =
        store::open("default").and_then(|bucket| bucket.set("warmup", seen.as_bytes()))
    {
        return Answer::new(507, kv_message(&error));
    }
    match config::get("WARMUP") {
        Ok(Some(mode)) if mode == "trap" => {
            #[expect(clippy::panic, reason = "the warm-up exists to trap")]
            {
                panic!("fn_misbehave traps on warm-up")
            }
        }
        Ok(Some(mode)) if mode == "fail" => Answer::new(500, "warm-up fails"),
        Ok(Some(mode)) if mode == "spin" => spin(),
        _ => Answer::ok(),
    }
}

fn grow(query: &str) -> Answer {
    let Some(bytes) = param(query, "bytes").and_then(|raw| raw.parse::<usize>().ok()) else {
        return Answer::new(400, "bytes missing");
    };
    let filled = vec![1_u8; bytes];
    core::hint::black_box(&filled);
    Answer::ok()
}

const fn spin() -> ! {
    loop {
        core::hint::black_box(());
    }
}

/// Reads the file `path` of the query: a function has no files, so this
/// answers `403` with the error.
fn files(query: &str) -> Answer {
    let path = param(query, "path").unwrap_or("/");
    match std::fs::read(path) {
        Ok(read) => Answer::new(200, read),
        Err(error) => Answer::new(403, error.to_string()),
    }
}

fn sleep(query: &str) -> Answer {
    let Some(ms) = param(query, "ms").and_then(|raw| raw.parse::<u64>().ok()) else {
        return Answer::new(400, "ms missing");
    };
    monotonic_clock::subscribe_duration(ms.saturating_mul(1_000_000)).block();
    Answer::ok()
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

/// `GET url` (`http://authority/path`) through the host.
fn fetch(url: &str) -> Result<Answer, ErrorCode> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| ErrorCode::InternalError(Some("no scheme".to_owned())))?;
    let (authority, path) = rest.find('/').map_or((rest, "/"), |at| rest.split_at(at));
    let request = OutgoingRequest::new(Fields::new());
    let scheme = if scheme == "https" {
        Scheme::Https
    } else {
        Scheme::Http
    };
    let refused = |what: &str| ErrorCode::InternalError(Some(format!("bad {what}")));
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
        .ok_or_else(|| ErrorCode::InternalError(Some("no response".to_owned())))?
        .map_err(|()| ErrorCode::InternalError(Some("response taken".to_owned())))??;
    let status = response.status();
    let body = response
        .consume()
        .map_err(|()| ErrorCode::InternalError(Some("body taken".to_owned())))?;
    let read = body
        .stream()
        .map(|stream| read_all(&stream))
        .unwrap_or_default();
    let _trailers = IncomingBody::finish(body);
    Ok(Answer::new(status, read))
}

/// What a `wasi:keyvalue` error says.
fn kv_message(error: &store::Error) -> String {
    match error {
        store::Error::NoSuchStore => "no-such-store".to_owned(),
        store::Error::AccessDenied => "access-denied".to_owned(),
        store::Error::Other(message) => message.clone(),
    }
}

/// The `/kv/` paths; `Err` is a `wasi:keyvalue` error's message.
fn keyvalue(
    request: &IncomingRequest,
    method: &Method,
    path: &str,
    query: &str,
) -> Result<Answer, String> {
    if path == "/kv/open" {
        let name = param(query, "name").unwrap_or_default();
        return store::open(name)
            .map(|_bucket| Answer::ok())
            .map_err(|error| kv_message(&error));
    }
    let bucket = store::open("default").map_err(|error| kv_message(&error))?;
    let key = param(query, "key").unwrap_or_default();
    let fail = |error: store::Error| kv_message(&error);
    match (method, path) {
        (Method::Get, "/kv/get") => Ok(bucket
            .get(key)
            .map_err(fail)?
            .map_or_else(Answer::not_found, |value| Answer::new(200, value))),
        (Method::Post, "/kv/set") => {
            bucket.set(key, &request_body(request)).map_err(fail)?;
            Ok(Answer::new(204, ""))
        }
        (Method::Post, "/kv/delete") => {
            bucket.delete(key).map_err(fail)?;
            Ok(Answer::new(204, ""))
        }
        (Method::Get, "/kv/incr") => {
            let value = atomics::increment(&bucket, key, 1).map_err(fail)?;
            Ok(Answer::new(200, value.to_string()))
        }
        (Method::Post, "/kv/set-many") => {
            let body = String::from_utf8_lossy(&request_body(request)).into_owned();
            let pairs: Vec<(String, Vec<u8>)> = body
                .split('&')
                .filter_map(|pair| pair.split_once('='))
                .map(|(k, v)| (k.to_owned(), v.as_bytes().to_vec()))
                .collect();
            batch::set_many(&bucket, &pairs).map_err(fail)?;
            Ok(Answer::new(204, ""))
        }
        (Method::Get, "/kv/get-many") => {
            let keys: Vec<String> = param(query, "keys")
                .unwrap_or_default()
                .split(',')
                .map(str::to_owned)
                .collect();
            let values = batch::get_many(&bucket, &keys).map_err(fail)?;
            let joined: Vec<String> = values
                .into_iter()
                .map(|found| {
                    found.map_or_else(
                        || "-".to_owned(),
                        |(_, value)| String::from_utf8_lossy(&value).into_owned(),
                    )
                })
                .collect();
            Ok(Answer::new(200, joined.join(",")))
        }
        _ => Ok(Answer::not_found()),
    }
}

/// Sends `bytes` zero bytes as the response, 4 KiB at a time.
fn stream_zeros(response_out: ResponseOutparam, bytes: u64) {
    let response = OutgoingResponse::new(Fields::new());
    let Ok(body) = response.body() else {
        return;
    };
    ResponseOutparam::set(response_out, Ok(response));
    if let Ok(stream) = body.write() {
        let chunk = [0_u8; 4096];
        let mut left = bytes;
        while left > 0 {
            let size = usize::try_from(left.min(4096)).unwrap_or(4096);
            if stream
                .blocking_write_and_flush(chunk.get(..size).unwrap_or(&chunk))
                .is_err()
            {
                break;
            }
            left = left.saturating_sub(4096);
        }
        drop(stream);
    }
    let _finished: Result<(), ErrorCode> = OutgoingBody::finish(body, None);
}

/// Sends `answer` as the response, `text/plain`.
fn respond(response_out: ResponseOutparam, answer: &Answer) {
    let headers = Fields::from_list(&[("content-type".to_owned(), b"text/plain".to_vec())])
        .unwrap_or_else(|_| Fields::new());
    let response = OutgoingResponse::new(headers);
    let _status: Result<(), ()> = response.set_status_code(answer.status);
    let Ok(body) = response.body() else {
        return;
    };
    ResponseOutparam::set(response_out, Ok(response));
    if let Ok(stream) = body.write() {
        for chunk in answer.body.chunks(4096) {
            if stream.blocking_write_and_flush(chunk).is_err() {
                break;
            }
        }
        drop(stream);
    }
    let _finished: Result<(), ErrorCode> = OutgoingBody::finish(body, None);
}

/// The exported symbols `wit-bindgen` generates for [`Misbehave`].
#[expect(unsafe_code, reason = "the export glue wit-bindgen generates")]
mod export {
    use super::Misbehave;

    crate::bindings::export!(Misbehave with_types_in crate::bindings);
}
