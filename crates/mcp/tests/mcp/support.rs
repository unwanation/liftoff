//! Shared fixture: a recording stub of the liftoff API and an MCP client
//! connected to `liftoff_mcp::serve` over in-memory pipes.

use std::sync::{Arc, Mutex, MutexGuard};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::response::Response;
use liftoff_mcp::Session;
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt as _};

/// The session token `lof mcp` read from the CLI config.
pub(crate) const TOKEN: &str = "lof_session_7f3a";

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A request the stub saw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Seen {
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) query: String,
    pub(crate) authorization: Option<String>,
    pub(crate) body: String,
}

impl Seen {
    /// The JSON body (`null` when there is none).
    pub(crate) fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).unwrap_or(serde_json::Value::Null)
    }

    /// The query string as sorted `key=value` pairs.
    pub(crate) fn query_pairs(&self) -> Vec<String> {
        let mut pairs: Vec<String> = self
            .query
            .split('&')
            .filter(|pair| !pair.is_empty())
            .map(ToOwned::to_owned)
            .collect();
        pairs.sort_unstable();
        pairs
    }
}

/// What the stub answers.
#[derive(Debug, Clone)]
pub(crate) struct Answer {
    status: u16,
    content_type: &'static str,
    body: String,
}

/// `status` with a JSON `body`.
pub(crate) fn json(status: u16, body: &serde_json::Value) -> Answer {
    Answer {
        status,
        content_type: "application/json",
        body: body.to_string(),
    }
}

/// `200` with plain text.
pub(crate) fn text(body: &str) -> Answer {
    Answer {
        status: 200,
        content_type: "text/plain; charset=utf-8",
        body: body.to_owned(),
    }
}

/// `200` with a server-sent-events body.
pub(crate) fn events(body: &str) -> Answer {
    Answer {
        status: 200,
        content_type: "text/event-stream",
        body: body.to_owned(),
    }
}

/// An RFC 9457 problem with a `detail`.
pub(crate) fn problem(status: u16, detail: &str) -> Answer {
    Answer {
        status,
        content_type: "application/problem+json",
        body: serde_json::json!({"type": "about:blank", "title": "Not Found", "status": status, "detail": detail})
            .to_string(),
    }
}

#[derive(Debug)]
struct Rule {
    method: &'static str,
    path: String,
    answer: Answer,
}

#[derive(Debug, Default)]
struct Shared {
    rules: Mutex<Vec<Rule>>,
    seen: Mutex<Vec<Seen>>,
}

/// The local API stub. `GET /api/v1/modules` answers `[]` unless a test
/// routes it.
#[derive(Debug)]
pub(crate) struct Stub {
    shared: Arc<Shared>,
    /// `http://127.0.0.1:<port>`
    pub(crate) url: String,
}

impl Stub {
    /// Starts a stub with no routes but `GET /modules` (everything else
    /// is `404`).
    pub(crate) async fn start() -> Self {
        let shared = Arc::new(Shared::default());
        let app = Router::new()
            .fallback(handle)
            .with_state(Arc::clone(&shared));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        drop(tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }));
        Self { shared, url }
    }

    /// Answers `method /api/v1<path>` with `answer`.
    pub(crate) fn route(self, method: &'static str, path: &str, answer: Answer) -> Self {
        lock(&self.shared.rules).push(Rule {
            method,
            path: format!("/api/v1{path}"),
            answer,
        });
        self
    }

    /// The requests seen, without the `GET /api/v1/modules` the server
    /// lists the tools of modules with.
    pub(crate) fn calls(&self) -> Vec<Seen> {
        lock(&self.shared.seen)
            .iter()
            .filter(|seen| !(seen.method == "GET" && seen.path == "/api/v1/modules"))
            .cloned()
            .collect()
    }
}

async fn handle(State(shared): State<Arc<Shared>>, request: Request) -> Response {
    let method = request.method().as_str().to_owned();
    let path = request.uri().path().to_owned();
    let query = request.uri().query().unwrap_or_default().to_owned();
    let authorization = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned);
    let bytes = to_bytes(request.into_body(), usize::MAX).await.unwrap();
    let body = String::from_utf8_lossy(&bytes).into_owned();
    lock(&shared.seen).push(Seen {
        method: method.clone(),
        path: path.clone(),
        query,
        authorization,
        body,
    });
    let answer = lock(&shared.rules)
        .iter()
        .rev()
        .find(|rule| rule.method == method && rule.path == path)
        .map(|rule| rule.answer.clone())
        .or_else(|| {
            (method == "GET" && path == "/api/v1/modules")
                .then(|| json(200, &serde_json::json!([])))
        })
        .unwrap_or_else(|| problem(404, &format!("no route {method} {path}")));
    Response::builder()
        .status(StatusCode::from_u16(answer.status).unwrap())
        .header(header::CONTENT_TYPE, answer.content_type)
        .body(Body::from(answer.body))
        .unwrap()
}

/// An MCP client of a server started on `stub` with [`TOKEN`].
pub(crate) type Client = RunningService<RoleClient, ()>;

/// Starts `liftoff_mcp::serve` for `stub` over a pair of pipes standing
/// in for stdin and stdout, and connects an MCP client to the other ends.
pub(crate) async fn connect(stub: &Stub) -> Client {
    let (client_end, server_end) = tokio::io::duplex(1 << 16);
    let (server_in, server_out) = tokio::io::split(server_end);
    let session = Session {
        url: stub.url.clone(),
        token: TOKEN.to_owned(),
    };
    drop(tokio::spawn(liftoff_mcp::serve(
        session, server_in, server_out,
    )));
    let (client_in, client_out) = tokio::io::split(client_end);
    ().serve((client_in, client_out))
        .await
        .expect("the MCP server answers initialize")
}

/// The arguments of a tool call.
pub(crate) fn args(value: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    match value {
        serde_json::Value::Object(map) => map,
        other => panic!("arguments must be an object, got {other}"),
    }
}

/// The name of the tool of the API operation `method path`.
pub(crate) fn tool_of(method: &str, path: &str) -> String {
    let operation = liftoff_api::OPERATIONS
        .iter()
        .find(|operation| operation.method.as_str() == method && operation.path == path)
        .unwrap_or_else(|| panic!("no operation {method} {path} in liftoff_api::OPERATIONS"));
    format!("liftoff_{}", operation.name)
}
