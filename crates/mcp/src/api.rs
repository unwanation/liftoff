//! The REST side: one request of the liftoff API with the session of the
//! CLI config, and its answer as the text of a tool result.

use core::time::Duration;

use liftoff_api::Method;
use serde_json::Value;

use crate::Session;

/// How long a connection to the daemon may take to open.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long one call may take in all: the daemon's slow routes answer
/// within 150 s, as for the CLI.
const REQUEST_TIMEOUT: Duration = Duration::from_mins(3);
/// The largest answer a call reads; a tool result is read by an agent,
/// and a larger one is refused instead of buffered.
pub(crate) const MAX_ANSWER: usize = 4 << 20;

/// One request of the API, built from a tool call or a resource URI.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Call {
    pub(crate) method: Method,
    /// The path with its parameters filled in, `/api/v1` included.
    pub(crate) path: String,
    pub(crate) query: Vec<(String, String)>,
    /// The JSON body, if the operation takes one.
    pub(crate) body: Option<Value>,
}

impl Call {
    /// A `GET` of `path` without a query.
    pub(crate) const fn get(path: String) -> Self {
        Self {
            method: Method::Get,
            path,
            query: Vec::new(),
            body: None,
        }
    }
}

/// A successful answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Success {
    pub(crate) status: u16,
    pub(crate) body: String,
}

/// Why a call failed, worded for the agent that made it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Failure {
    /// The API answered with an error status (an RFC 9457 problem as a
    /// rule).
    Refused {
        status: u16,
        /// `<status> <title>: <detail>`.
        text: String,
    },
    /// The server could not be reached or its answer not read.
    Unreachable(String),
}

impl Failure {
    /// The text of the tool error.
    pub(crate) fn text(&self) -> &str {
        match self {
            Self::Refused { text, .. } | Self::Unreachable(text) => text,
        }
    }
}

/// The API of one installation, called with one session.
#[derive(Debug, Clone)]
pub(crate) struct Api {
    http: reqwest::Client,
    /// Base URL without a trailing `/`.
    url: String,
    token: String,
}

impl Api {
    /// The API of `session`, with the timeouts of the CLI.
    ///
    /// # Errors
    ///
    /// The HTTP client cannot start (its TLS backend fails to load).
    pub(crate) fn new(session: Session) -> Result<Self, String> {
        Self::with_timeouts(session, CONNECT_TIMEOUT, REQUEST_TIMEOUT)
    }

    /// [`Api::new`] with other timeouts (tests).
    fn with_timeouts(
        session: Session,
        connect: Duration,
        request: Duration,
    ) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .connect_timeout(connect)
            .timeout(request)
            .user_agent(concat!("liftoff-mcp/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| format!("cannot start the HTTP client: {error}"))?;
        Ok(Self {
            http,
            url: session.url.trim_end_matches('/').to_owned(),
            token: session.token,
        })
    }

    /// The API of `session` over this one's HTTP client.
    pub(crate) fn with_session(&self, session: &Session) -> Self {
        Self {
            http: self.http.clone(),
            url: session.url.trim_end_matches('/').to_owned(),
            token: session.token.clone(),
        }
    }

    /// Sends `call` with the session as `Authorization: Bearer`.
    pub(crate) async fn send(&self, call: &Call) -> Result<Success, Failure> {
        let method = match call.method {
            Method::Get => reqwest::Method::GET,
            Method::Post => reqwest::Method::POST,
            Method::Put => reqwest::Method::PUT,
            Method::Patch => reqwest::Method::PATCH,
            Method::Delete => reqwest::Method::DELETE,
        };
        let mut request = self
            .http
            .request(method, format!("{}{}", self.url, call.path))
            .bearer_auth(&self.token);
        if !call.query.is_empty() {
            request = request.query(&call.query);
        }
        if let Some(body) = &call.body {
            request = request.json(body);
        }
        let unreachable = |error: reqwest::Error| {
            Failure::Unreachable(format!("cannot reach {}: {error}", self.url))
        };
        let mut response = request.send().await.map_err(unreachable)?;
        let status = response.status();
        let mut bytes: Vec<u8> = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(unreachable)? {
            if bytes.len().saturating_add(chunk.len()) > MAX_ANSWER {
                return Err(Failure::Unreachable(format!(
                    "the answer of {} {} is larger than {MAX_ANSWER} bytes",
                    call.method.as_str(),
                    call.path
                )));
            }
            bytes.extend_from_slice(&chunk);
        }
        let body = String::from_utf8_lossy(&bytes).into_owned();
        if status.is_success() {
            return Ok(Success {
                status: status.as_u16(),
                body,
            });
        }
        Err(Failure::Refused {
            status: status.as_u16(),
            text: refusal(status, &body),
        })
    }
}

/// `<status> <title>: <detail>` of a problem, or the status and the raw
/// body of anything else.
fn refusal(status: reqwest::StatusCode, body: &str) -> String {
    let problem: Option<Value> = serde_json::from_str(body).ok();
    let field = |name: &str| {
        problem
            .as_ref()
            .and_then(|problem| problem.get(name))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
    };
    let title = field("title")
        .or_else(|| status.canonical_reason().map(ToOwned::to_owned))
        .unwrap_or_default();
    match (field("detail"), body.trim()) {
        (Some(detail), _) => format!("{} {title}: {detail}", status.as_u16()),
        (None, "") => format!("{} {title}", status.as_u16()),
        (None, raw) if problem.is_none() => format!("{} {title}: {raw}", status.as_u16()),
        (None, _) => format!("{} {title}", status.as_u16()),
    }
}

/// The `data:` lines of a server-sent-events body, one per line: the
/// text of a log stream.
pub(crate) fn events_text(body: &str) -> String {
    let mut lines = Vec::new();
    let mut error = false;
    for line in body.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            error = false;
            continue;
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => error = value == "error",
            "data" if error => lines.push(format!("error: {value}")),
            "data" => lines.push(value.to_owned()),
            _ => {}
        }
    }
    let mut text = lines.join("\n");
    if !text.is_empty() {
        text.push('\n');
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    mod send {
        use super::*;

        // TC: MCP-03 (a server that accepts and never answers fails the
        // call instead of hanging it)
        #[tokio::test]
        async fn unreachable_when_server_never_answers() {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .unwrap_or_else(|error| panic!("bind: {error}"));
            let url = listener
                .local_addr()
                .map(|address| format!("http://{address}"))
                .unwrap_or_default();
            let held = tokio::spawn(async move {
                let accepted = listener.accept().await;
                tokio::time::sleep(Duration::from_secs(30)).await;
                drop(accepted);
            });
            let api = Api::with_timeouts(
                Session {
                    url,
                    token: "t".to_owned(),
                },
                Duration::from_millis(200),
                Duration::from_millis(200),
            )
            .unwrap_or_else(|error| panic!("{error}"));
            let answered = tokio::time::timeout(
                Duration::from_secs(10),
                api.send(&Call::get("/api/v1/projects".to_owned())),
            )
            .await;
            held.abort();
            assert!(
                matches!(answered, Ok(Err(Failure::Unreachable(_)))),
                "{answered:?}"
            );
        }
    }

    mod events_text {
        use super::*;

        // TC: MCP-05 (a log resource is the data lines of its stream)
        #[test]
        fn data_lines_joined_when_stream_has_events() {
            assert_eq!(
                events_text(": keep-alive\n\ndata: one\n\ndata: two\n\n"),
                "one\ntwo\n"
            );
        }

        // TC: MCP-05 (a stream that ends in an error says so)
        #[test]
        fn error_marked_when_stream_fails() {
            assert_eq!(
                events_text("data: one\n\nevent: error\ndata: the log stream failed\n\n"),
                "one\nerror: the log stream failed\n"
            );
        }
    }

    mod refusal {
        use super::*;

        // TC: MCP-03 (the tool error carries the status and the detail)
        #[test]
        fn status_title_and_detail_when_body_is_problem() {
            let body = r#"{"type":"about:blank","title":"Not Found","status":404,"detail":"project ghost not found"}"#;
            assert_eq!(
                refusal(reqwest::StatusCode::NOT_FOUND, body),
                "404 Not Found: project ghost not found"
            );
        }

        // TC: MCP-03 (an error body that is no problem is passed on as it is)
        #[test]
        fn raw_body_when_not_json() {
            assert_eq!(
                refusal(reqwest::StatusCode::BAD_GATEWAY, "upstream down\n"),
                "502 Bad Gateway: upstream down"
            );
        }
    }
}
