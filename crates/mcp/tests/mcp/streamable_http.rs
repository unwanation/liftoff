//! The Streamable HTTP transport (`liftoff_mcp::http`; MCPH-01, 03, 04, 06) against a hand-written [`Backend`] and the
//! recording API stub: the transport refuses what the backend refuses,
//! with the answers MCP clients expect, and its tools call the API with
//! the session the backend granted. Driven in memory with hand-written
//! JSON-RPC (protocol `2025-06-18`); an answer is read as JSON or as the
//! `data:` of its server-sent events, whichever the transport chose. The
//! daemon's backend over the core is `crates/liftoffd/tests/mcp_http.rs`.

use core::time::Duration;
use std::sync::{Arc, Mutex};

use http::{HeaderMap, Request, StatusCode, header};
use http_body_util::BodyExt as _;
use liftoff_mcp::Session;
use liftoff_mcp::http::{Backend, Grant, HttpConfig, Refusal, StreamableHttp};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt as _;

use crate::support::{Stub, json as answer, tool_of};

/// The API host the transport answers.
const HOST: &str = "liftoff.test";
/// The resource metadata the transport names.
const METADATA: &str = "https://liftoff.test/.well-known/oauth-protected-resource";
/// The access token of `bob` the fake backend grants for a day.
const GOOD: &str = "at-good";
/// Another access token of `bob`, granted for a day too.
const GOOD_TOO: &str = "at-good-too";
/// The access token of `carol` the fake backend grants for a day.
const CAROL: &str = "at-carol";
/// An access token of `bob` granted for [`SHORT_LIFE`] only.
const SHORT: &str = "at-short";
/// An access token of `bob` granted for [`BRIEF_LIFE`] of real time:
/// the event stream of `rmcp` keeps itself alive by the wall clock, so
/// the test that reads one runs in real time.
const BRIEF: &str = "at-brief";
/// The access token the fake backend refuses as invalid.
const BAD: &str = "at-bad";
/// The access token of a removed module.
const REMOVED: &str = "at-removed";
/// The access token of a person who may not use liftoff.
const FORBIDDEN: &str = "at-forbidden";
/// An access token the backend cannot check now.
const DOWN: &str = "at-down";
/// The API session the backend grants for [`GOOD`].
const SESSION: &str = "lfs_mcp_bob";
const PROTOCOL: &str = "2025-06-18";
/// How long the API session of [`SHORT`] lasts.
const SHORT_LIFE: Duration = Duration::from_hours(1);
/// How long the API session of [`BRIEF`] lasts.
const BRIEF_LIFE: Duration = Duration::from_millis(300);
/// How long the other grants last.
const DAY: Duration = Duration::from_hours(24);
/// What the backend logs for [`DOWN`]: never for the client.
const DOWN_REASON: &str = "module oidc trapped at 0x1f";

/// A backend that grants [`GOOD`], [`GOOD_TOO`] and [`SHORT`] to `bob`
/// and [`CAROL`] to `carol` (sessions of `url`), refuses [`BAD`] and a
/// missing bearer, answers [`REMOVED`] with the module `mcp` required,
/// [`FORBIDDEN`] with a refusal and [`DOWN`] as unavailable; it records
/// every bearer it was asked about.
#[derive(Debug, Clone)]
struct FakeBackend {
    url: String,
    asked: Arc<Mutex<Vec<Option<String>>>>,
}

impl FakeBackend {
    fn new(url: &str) -> Self {
        Self {
            url: url.to_owned(),
            asked: Arc::default(),
        }
    }

    fn asked(&self) -> Vec<Option<String>> {
        self.asked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn grant(&self, principal: &str, token: &str, valid_for: Duration) -> Grant {
        Grant {
            session: Session {
                url: self.url.clone(),
                token: token.to_owned(),
            },
            principal: principal.to_owned(),
            valid_for,
        }
    }
}

impl Backend for FakeBackend {
    fn session(&self, bearer: Option<&str>) -> impl Future<Output = Result<Grant, Refusal>> + Send {
        self.asked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(bearer.map(str::to_owned));
        core::future::ready(match bearer {
            Some(GOOD | GOOD_TOO) => Ok(self.grant("bob", SESSION, DAY)),
            Some(SHORT) => Ok(self.grant("bob", SESSION, SHORT_LIFE)),
            Some(BRIEF) => Ok(self.grant("bob", SESSION, BRIEF_LIFE)),
            Some(CAROL) => Ok(self.grant("carol", "lfs_mcp_carol", DAY)),
            Some(REMOVED) => Err(Refusal::ModuleRequired("mcp".to_owned())),
            Some(FORBIDDEN) => Err(Refusal::Forbidden(
                "another user has the login bob".to_owned(),
            )),
            Some(DOWN) => Err(Refusal::Unavailable(DOWN_REASON.to_owned())),
            Some(_) => Err(Refusal::Unauthenticated(
                "the audience is not allowed".to_owned(),
            )),
            None => Err(Refusal::Unauthenticated("no bearer".to_owned())),
        })
    }
}

/// The transport's set-up in tests: idle sessions are kept for a week,
/// so only the bounds under test close them.
fn config(max_sessions: usize, per_principal: usize, shutdown: &CancellationToken) -> HttpConfig {
    HttpConfig {
        resource_metadata: METADATA.to_owned(),
        allowed_hosts: vec![HOST.to_owned()],
        max_sessions,
        max_sessions_per_principal: per_principal,
        session_idle: DAY.saturating_mul(7),
        shutdown: shutdown.clone(),
    }
}

fn transport(backend: FakeBackend) -> StreamableHttp<FakeBackend> {
    StreamableHttp::new(backend, config(16, 16, &CancellationToken::new()))
}

/// An answer of the transport.
struct Answer {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

impl Answer {
    fn header(&self, name: &str) -> Option<String> {
        self.headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    }

    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }

    /// The JSON-RPC response of the answer (JSON, or the last `data:` of
    /// its event stream that has an `id`).
    fn message(&self) -> Value {
        if !self
            .header("content-type")
            .is_some_and(|kind| kind.starts_with("text/event-stream"))
        {
            return self.json();
        }
        self.body
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .filter_map(|data| serde_json::from_str::<Value>(data.trim()).ok())
            .rfind(|message| message.get("id").is_some())
            .unwrap_or(Value::Null)
    }
}

/// `method /mcp` with `bearer`, the transport session and `message`.
async fn send(
    mcp: &StreamableHttp<FakeBackend>,
    method: &str,
    bearer: Option<&str>,
    session: Option<&str>,
    message: Option<&Value>,
) -> Answer {
    let mut request = Request::builder()
        .method(method)
        .uri("/mcp")
        .header(header::HOST, HOST)
        .header(header::ACCEPT, "application/json, text/event-stream");
    if message.is_some() {
        request = request.header(header::CONTENT_TYPE, "application/json");
    }
    if let Some(token) = bearer {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    if let Some(id) = session {
        request = request
            .header("mcp-session-id", id)
            .header("mcp-protocol-version", PROTOCOL);
    }
    let body = message.map_or_else(String::new, Value::to_string);
    let response = mcp
        .clone()
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    Answer {
        status,
        headers,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

/// `POST /mcp` with `message`, `bearer` and the transport session.
async fn post(
    mcp: &StreamableHttp<FakeBackend>,
    bearer: Option<&str>,
    session: Option<&str>,
    message: &Value,
) -> Answer {
    send(mcp, "POST", bearer, session, Some(message)).await
}

/// `tools/list` in `session` with `bearer`.
async fn list_tools(mcp: &StreamableHttp<FakeBackend>, bearer: &str, session: &str) -> Answer {
    post(
        mcp,
        Some(bearer),
        Some(session),
        &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
    )
    .await
}

/// `GET /mcp` (the event stream of `session`) with `bearer`: the answer,
/// its body still open.
async fn open_event_stream(
    mcp: &StreamableHttp<FakeBackend>,
    bearer: &str,
    session: &str,
) -> http::Response<liftoff_mcp::http::HttpBody> {
    let request = Request::builder()
        .method("GET")
        .uri("/mcp")
        .header(header::HOST, HOST)
        .header(header::ACCEPT, "text/event-stream")
        .header(header::AUTHORIZATION, format!("Bearer {bearer}"))
        .header("mcp-session-id", session)
        .header("mcp-protocol-version", PROTOCOL)
        .body(String::new())
        .unwrap();
    mcp.clone().oneshot(request).await.unwrap()
}

/// Reads the body of `response` to its end, which must come within
/// `within`; the answer's status.
async fn read_to_end(
    response: http::Response<liftoff_mcp::http::HttpBody>,
    within: Duration,
) -> StatusCode {
    let status = response.status();
    tokio::time::timeout(within, response.into_body().collect())
        .await
        .expect("the event stream did not end")
        .unwrap();
    status
}

fn initialize() -> Value {
    json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": PROTOCOL,
            "capabilities": {},
            "clientInfo": {"name": "mcph-test", "version": "0.1.0"},
        },
    })
}

/// Initializes with [`GOOD`]: the transport session, if the transport
/// keeps one.
async fn connect(mcp: &StreamableHttp<FakeBackend>) -> Option<String> {
    connect_as(mcp, GOOD).await
}

/// Initializes with `bearer`: the transport session.
async fn connect_as(mcp: &StreamableHttp<FakeBackend>, bearer: &str) -> Option<String> {
    let answer = post(mcp, Some(bearer), None, &initialize()).await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    let session = answer.header("mcp-session-id");
    let initialized = post(
        mcp,
        Some(bearer),
        session.as_deref(),
        &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    )
    .await;
    assert_eq!(
        initialized.status,
        StatusCode::ACCEPTED,
        "{}",
        initialized.body
    );
    session
}

mod refusal {
    use super::*;

    // TC: MCPH-01 (no bearer: 401 naming the resource metadata, RFC 9728
    // 5.1)
    #[tokio::test]
    async fn answers_401_with_resource_metadata_when_no_bearer() {
        let mcp = transport(FakeBackend::new("http://127.0.0.1:9"));
        let answer = post(&mcp, None, None, &initialize()).await;
        assert_eq!(
            (answer.status, answer.header("www-authenticate")),
            (
                StatusCode::UNAUTHORIZED,
                Some(format!("Bearer resource_metadata=\"{METADATA}\""))
            ),
            "{}",
            answer.body
        );
    }

    // TC: MCPH-03 (a token the backend refuses: 401, with the metadata)
    #[tokio::test]
    async fn answers_401_with_resource_metadata_when_token_refused() {
        let mcp = transport(FakeBackend::new("http://127.0.0.1:9"));
        let answer = post(&mcp, Some(BAD), None, &initialize()).await;
        assert_eq!(
            (
                answer.status,
                answer
                    .header("www-authenticate")
                    .is_some_and(|value| value.starts_with("Bearer ")
                        && value.contains(&format!("resource_metadata=\"{METADATA}\""))),
            ),
            (StatusCode::UNAUTHORIZED, true),
            "{}",
            answer.body
        );
    }

    // TC: MCPH-03 (the refusal's reason is for the log, not the client)
    #[tokio::test]
    async fn keeps_refusal_reason_out_of_answer_when_token_refused() {
        let mcp = transport(FakeBackend::new("http://127.0.0.1:9"));
        let answer = post(&mcp, Some(BAD), None, &initialize()).await;
        assert_eq!(
            (answer.status, answer.body.contains("audience")),
            (StatusCode::UNAUTHORIZED, false),
            "{}",
            answer.body
        );
    }

    // TC: MCPH-03 (a refused bearer is an invalid token, RFC 6750 3.1)
    #[tokio::test]
    async fn answers_invalid_token_challenge_when_token_refused() {
        let mcp = transport(FakeBackend::new("http://127.0.0.1:9"));
        let answer = post(&mcp, Some(BAD), None, &initialize()).await;
        assert_eq!(
            answer.header("www-authenticate"),
            Some(format!(
                "Bearer resource_metadata=\"{METADATA}\", error=\"invalid_token\""
            )),
            "{}",
            answer.body
        );
    }

    // TC: MCPH-04 (a valid token of a person liftoff refuses: 403 problem
    // with the core's reason, as the REST API answers it)
    #[tokio::test]
    async fn answers_403_problem_when_backend_forbids() {
        let mcp = transport(FakeBackend::new("http://127.0.0.1:9"));
        let answer = post(&mcp, Some(FORBIDDEN), None, &initialize()).await;
        assert_eq!(
            (
                answer.status,
                answer.header("content-type"),
                answer.json()["detail"].clone()
            ),
            (
                StatusCode::FORBIDDEN,
                Some("application/problem+json".to_owned()),
                json!("another user has the login bob")
            ),
            "{}",
            answer.body
        );
    }

    // TC: MCPH-03 (the check cannot be made: 503, its cause in the log
    // alone)
    #[tokio::test]
    async fn answers_503_without_reason_when_backend_unavailable() {
        let mcp = transport(FakeBackend::new("http://127.0.0.1:9"));
        let answer = post(&mcp, Some(DOWN), None, &initialize()).await;
        assert_eq!(
            (answer.status, answer.body.contains(DOWN_REASON)),
            (StatusCode::SERVICE_UNAVAILABLE, false),
            "{}",
            answer.body
        );
    }

    // TC: MCPH-06 (the module is gone: 424 problem)
    #[tokio::test]
    async fn answers_424_problem_when_backend_requires_module() {
        let mcp = transport(FakeBackend::new("http://127.0.0.1:9"));
        let answer = post(&mcp, Some(REMOVED), None, &initialize()).await;
        assert_eq!(
            (
                answer.status,
                answer.header("content-type"),
                answer.json()["detail"].clone()
            ),
            (
                StatusCode::FAILED_DEPENDENCY,
                Some("application/problem+json".to_owned()),
                json!("install the module `mcp`")
            ),
            "{}",
            answer.body
        );
    }
}

mod granted {
    use super::*;

    // TC: MCPH-04 (the backend is asked about the request's own bearer)
    #[tokio::test]
    async fn asks_backend_with_request_bearer_when_initialized() {
        let backend = FakeBackend::new("http://127.0.0.1:9");
        let mcp = transport(backend.clone());
        let _answer = post(&mcp, Some(GOOD), None, &initialize()).await;
        assert_eq!(backend.asked(), vec![Some(GOOD.to_owned())]);
    }

    // TC: MCPH-04 (the tools call the API with the granted session)
    #[tokio::test]
    async fn sends_granted_session_as_bearer_when_tool_called() {
        let stub = Stub::start().await.route(
            "GET",
            "/me",
            answer(200, &json!({"id": "01JA0000000000000000000002", "login": "bob", "email": "", "role": "user", "source": "oidc:https://id.example"})),
        );
        let mcp = transport(FakeBackend::new(&stub.url));
        let session = connect(&mcp).await;
        let called = post(
            &mcp,
            Some(GOOD),
            session.as_deref(),
            &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                    "params": {"name": tool_of("GET", "/api/v1/me"), "arguments": {}}}),
        )
        .await;
        assert_eq!(called.status, StatusCode::OK, "{}", called.body);
        let seen: Vec<(String, Option<String>)> = stub
            .calls()
            .into_iter()
            .map(|seen| (seen.path, seen.authorization))
            .collect();
        assert_eq!(
            seen,
            vec![("/api/v1/me".to_owned(), Some(format!("Bearer {SESSION}")))]
        );
    }

    // TC: MCPH-04 (the API's answer is the tool's result, as over stdio)
    #[tokio::test]
    async fn answers_api_json_when_tool_called() {
        let me = json!({"id": "01JA0000000000000000000002", "login": "bob", "email": "", "role": "user", "source": "oidc:https://id.example"});
        let stub = Stub::start().await.route("GET", "/me", answer(200, &me));
        let mcp = transport(FakeBackend::new(&stub.url));
        let session = connect(&mcp).await;
        let called = post(
            &mcp,
            Some(GOOD),
            session.as_deref(),
            &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                    "params": {"name": tool_of("GET", "/api/v1/me"), "arguments": {}}}),
        )
        .await;
        let result = called.message()["result"].clone();
        assert_eq!(
            (
                result["isError"].as_bool().unwrap_or(false),
                result["content"][0]["text"]
                    .as_str()
                    .and_then(|text| serde_json::from_str::<Value>(text).ok()),
            ),
            (false, Some(me)),
            "{}",
            called.body
        );
    }

    // TC: MCPH-04 (the same tools as stdio: the API's and the aliases)
    #[tokio::test]
    async fn lists_api_tools_and_aliases_when_granted() {
        let stub = Stub::start().await;
        let mcp = transport(FakeBackend::new(&stub.url));
        let session = connect(&mcp).await;
        let listed = post(
            &mcp,
            Some(GOOD),
            session.as_deref(),
            &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
        )
        .await;
        let message = listed.message();
        let names: Vec<&str> = message["result"]["tools"]
            .as_array()
            .map(|tools| {
                tools
                    .iter()
                    .filter_map(|tool| tool["name"].as_str())
                    .collect()
            })
            .unwrap_or_default();
        assert_eq!(
            (
                names.contains(&"liftoff_me_get"),
                names.contains(&"publish_artifact")
            ),
            (true, true),
            "{}",
            listed.body
        );
    }
}

mod sessions {
    use super::*;

    // TC: MCPH-04 (past the bound the oldest session is closed: its
    // client gets 404 and starts again)
    #[tokio::test]
    async fn closes_oldest_session_when_max_sessions_exceeded() {
        let mcp = StreamableHttp::new(
            FakeBackend::new("http://127.0.0.1:9"),
            config(1, 1, &CancellationToken::new()),
        );
        let first = connect(&mcp).await.unwrap();
        let _second = connect_as(&mcp, CAROL).await.unwrap();
        let answer = list_tools(&mcp, GOOD, &first).await;
        assert_eq!(answer.status, StatusCode::NOT_FOUND, "{}", answer.body);
    }

    // TC: MCPH-04 (a person past their own bound closes their oldest
    // session)
    #[tokio::test]
    async fn closes_principals_oldest_session_when_principal_bound_exceeded() {
        let mcp = StreamableHttp::new(
            FakeBackend::new("http://127.0.0.1:9"),
            config(16, 1, &CancellationToken::new()),
        );
        let first = connect(&mcp).await.unwrap();
        let _second = connect_as(&mcp, GOOD_TOO).await.unwrap();
        let answer = list_tools(&mcp, GOOD, &first).await;
        assert_eq!(answer.status, StatusCode::NOT_FOUND, "{}", answer.body);
    }

    // TC: MCPH-04 (one person cannot close the sessions of others)
    #[tokio::test]
    async fn keeps_other_principals_session_when_principal_bound_exceeded() {
        let mcp = StreamableHttp::new(
            FakeBackend::new("http://127.0.0.1:9"),
            config(16, 1, &CancellationToken::new()),
        );
        let carols = connect_as(&mcp, CAROL).await.unwrap();
        let _first = connect(&mcp).await.unwrap();
        let _second = connect_as(&mcp, GOOD_TOO).await.unwrap();
        let answer = list_tools(&mcp, CAROL, &carols).await;
        assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    }

    // TC: MCPH-04 (a session answers the person who opened it alone)
    #[tokio::test]
    async fn answers_404_when_other_principal_uses_session() {
        let mcp = transport(FakeBackend::new("http://127.0.0.1:9"));
        let bobs = connect(&mcp).await.unwrap();
        let answer = list_tools(&mcp, CAROL, &bobs).await;
        assert_eq!(answer.status, StatusCode::NOT_FOUND, "{}", answer.body);
    }

    // TC: MCPH-04 (another person's `DELETE` does not close it)
    #[tokio::test]
    async fn keeps_session_when_other_principal_deletes_it() {
        let mcp = transport(FakeBackend::new("http://127.0.0.1:9"));
        let bobs = connect(&mcp).await.unwrap();
        let deleted = send(&mcp, "DELETE", Some(CAROL), Some(&bobs), None).await;
        let answer = list_tools(&mcp, GOOD, &bobs).await;
        assert_eq!(
            (deleted.status, answer.status),
            (StatusCode::NOT_FOUND, StatusCode::OK),
            "{}",
            answer.body
        );
    }

    // TC: MCPH-04 (the opener's other tokens, a refreshed one, keep it)
    #[tokio::test]
    async fn answers_in_session_when_same_principal_uses_refreshed_token() {
        let mcp = transport(FakeBackend::new("http://127.0.0.1:9"));
        let bobs = connect(&mcp).await.unwrap();
        let answer = list_tools(&mcp, GOOD_TOO, &bobs).await;
        assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    }
}

mod lifetime {
    use super::*;

    // TC: MCPH-04 (a transport session ends with the API session it was
    // opened with: its open event stream too)
    #[tokio::test]
    async fn ends_event_stream_when_grant_expires() {
        let mcp = transport(FakeBackend::new("http://127.0.0.1:9"));
        let started = tokio::time::Instant::now();
        let session = connect_as(&mcp, BRIEF).await.unwrap();
        let stream = open_event_stream(&mcp, BRIEF, &session).await;
        let status = read_to_end(stream, Duration::from_secs(10)).await;
        assert_eq!(
            (status, started.elapsed() >= BRIEF_LIFE),
            (StatusCode::OK, true)
        );
    }

    // TC: MCPH-04 (the session is closed at that instant: later requests
    // get 404)
    #[tokio::test(start_paused = true)]
    async fn answers_404_when_grant_of_session_expired() {
        let mcp = transport(FakeBackend::new("http://127.0.0.1:9"));
        let session = connect_as(&mcp, SHORT).await.unwrap();
        tokio::time::sleep(SHORT_LIFE.saturating_add(Duration::from_secs(1))).await;
        let answer = list_tools(&mcp, GOOD, &session).await;
        assert_eq!(answer.status, StatusCode::NOT_FOUND, "{}", answer.body);
    }

    // TC: MCPH-04 (a request with a longer grant, a refreshed token, keeps
    // the session past the first one's end)
    #[tokio::test(start_paused = true)]
    async fn keeps_session_when_later_grant_lasts_longer() {
        let mcp = transport(FakeBackend::new("http://127.0.0.1:9"));
        let session = connect_as(&mcp, SHORT).await.unwrap();
        let _extended = list_tools(&mcp, GOOD, &session).await;
        tokio::time::sleep(SHORT_LIFE.saturating_add(Duration::from_secs(1))).await;
        let answer = list_tools(&mcp, GOOD, &session).await;
        assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    }

    // The daemon stopping ends the open event streams, so its graceful
    // shutdown does not wait on them.
    #[tokio::test]
    async fn ends_event_stream_when_shutdown_cancelled() {
        let shutdown = CancellationToken::new();
        let mcp = StreamableHttp::new(
            FakeBackend::new("http://127.0.0.1:9"),
            config(16, 16, &shutdown),
        );
        let session = connect(&mcp).await.unwrap();
        let stream = open_event_stream(&mcp, GOOD, &session).await;
        shutdown.cancel();
        let status = read_to_end(stream, Duration::from_secs(10)).await;
        assert_eq!(status, StatusCode::OK);
    }
}
