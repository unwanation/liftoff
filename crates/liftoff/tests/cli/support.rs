//! Shared fixture: a recording stub of the liftoff API and a way to run the
//! CLI against it with every effect injected (config path, working
//! directory, `git remote`, output, terminal and its answers, clock,
//! Ctrl-C).

use std::collections::VecDeque;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{FromRequest as _, Multipart, Request, State};
use axum::http::{StatusCode, header};
use axum::response::Response;
use liftoff::local::{Local, RegistryKey};
use liftoff::{Env, GitRemote, HostCommand, PromptError, Prompter, Sleeper, run};
use liftoff_tunnel::{Disconnect, ServerConfig, TunnelServer};
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_util::sync::CancellationToken;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A request the stub saw.
#[derive(Debug, Clone)]
pub(crate) struct Seen {
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) query: String,
    pub(crate) authorization: Option<String>,
    pub(crate) body: String,
    /// Text fields of a `multipart/form-data` body, in order (a binary
    /// field decoded lossily).
    pub(crate) fields: Vec<(String, String)>,
    /// Every field of a `multipart/form-data` body as bytes, in order.
    pub(crate) parts: Vec<(String, Vec<u8>)>,
    /// The `filename` of each field of [`Seen::parts`], in order (`None`
    /// for a field without one).
    pub(crate) filenames: Vec<Option<String>>,
    /// `X-Liftoff-Project` (the tunnel's upgrade sends it).
    pub(crate) project: Option<String>,
    /// The status the stub answered.
    pub(crate) status: u16,
}

impl Seen {
    /// The JSON body.
    pub(crate) fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).unwrap_or(serde_json::Value::Null)
    }

    /// The query string decoded into pairs, in order.
    pub(crate) fn query_pairs(&self) -> Vec<(String, String)> {
        self.query
            .split('&')
            .filter(|pair| !pair.is_empty())
            .map(|pair| {
                let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
                (decode(key), decode(value))
            })
            .collect()
    }

    /// The bytes of the multipart field `name`, if it was sent.
    pub(crate) fn part(&self, name: &str) -> Option<Vec<u8>> {
        self.parts
            .iter()
            .find(|(field, _)| field == name)
            .map(|(_, bytes)| bytes.clone())
    }

    /// `(filename, bytes)` of every multipart field `name` that carries a
    /// `filename`, in order (the files of `lof artifact publish`).
    pub(crate) fn uploads(&self, name: &str) -> Vec<(String, Vec<u8>)> {
        self.parts
            .iter()
            .zip(&self.filenames)
            .filter(|((field, _), _)| field == name)
            .filter_map(|((_, bytes), filename)| {
                filename
                    .as_ref()
                    .map(|filename| (filename.clone(), bytes.clone()))
            })
            .collect()
    }

    /// The value of `name`: a JSON body field (strings as is, anything else
    /// as JSON text), else a multipart field.
    pub(crate) fn param(&self, name: &str) -> Option<String> {
        let json = self.json();
        if let Some(value) = json.get(name) {
            return match value {
                serde_json::Value::String(text) => Some(text.clone()),
                serde_json::Value::Null => None,
                other => Some(other.to_string()),
            };
        }
        self.fields
            .iter()
            .find(|(field, _)| field == name)
            .map(|(_, value)| value.clone())
    }
}

/// Percent-decoding of a query component (`+` is a space).
fn decode(text: &str) -> String {
    let spaced = text.replace('+', " ");
    let mut parts = spaced.split('%');
    let mut out = parts.next().unwrap_or_default().as_bytes().to_vec();
    for part in parts {
        let (hex, rest) = part.split_at_checked(2).unwrap();
        out.push(u8::from_str_radix(hex, 16).unwrap());
        out.extend_from_slice(rest.as_bytes());
    }
    String::from_utf8(out).unwrap()
}

/// What the stub answers.
#[derive(Debug, Clone)]
pub(crate) struct Answer {
    status: u16,
    content_type: &'static str,
    body: String,
}

/// `status` with a JSON `body`.
pub(crate) fn json(status: u16, body: &str) -> Answer {
    Answer {
        status,
        content_type: "application/json",
        body: body.to_owned(),
    }
}

/// `status` with plain text.
pub(crate) fn text(status: u16, body: &str) -> Answer {
    Answer {
        status,
        content_type: "text/plain; charset=utf-8",
        body: body.to_owned(),
    }
}

/// A finished server-sent event stream.
pub(crate) fn sse(body: &str) -> Answer {
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
        body: serde_json::json!({"title": "Problem", "status": status, "detail": detail})
            .to_string(),
    }
}

#[derive(Debug)]
struct Rule {
    method: &'static str,
    path: String,
    answers: VecDeque<Answer>,
}

impl Rule {
    /// The next answer; the last one repeats.
    fn next(&mut self) -> Option<Answer> {
        if self.answers.len() > 1 {
            self.answers.pop_front()
        } else {
            self.answers.front().cloned()
        }
    }
}

#[derive(Debug, Default)]
struct Shared {
    rules: Mutex<Vec<Rule>>,
    seen: Mutex<Vec<Seen>>,
    /// `GET /tunnel` of [`Stub::start_with_tunnel`].
    tunnel: Option<Arc<FakeTunnel>>,
}

/// The local API stub.
#[derive(Debug)]
pub(crate) struct Stub {
    shared: Arc<Shared>,
    /// `http://127.0.0.1:<port>`
    pub(crate) url: String,
}

impl Stub {
    /// Starts a stub with no routes (everything is `404`).
    pub(crate) async fn start() -> Self {
        Self::serve(Arc::new(Shared::default())).await
    }

    /// Starts a stub with no routes whose `GET /tunnel` is the daemon's
    /// upgrade served by the real tunnel server (see [`FakeTunnel`]),
    /// and what that server tells of its connections.
    pub(crate) async fn start_with_tunnel(options: TunnelOptions) -> (Self, TunnelEvents) {
        let (events, received) = tokio::sync::mpsc::unbounded_channel();
        let tunnel = FakeTunnel {
            server: TunnelServer::new(ServerConfig::default()),
            options,
            events,
            connections: AtomicUsize::new(0),
        };
        let shared = Arc::new(Shared {
            tunnel: Some(Arc::new(tunnel)),
            ..Shared::default()
        });
        (Self::serve(shared).await, TunnelEvents(received))
    }

    async fn serve(shared: Arc<Shared>) -> Self {
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

    /// Answers `method` `/api/v1<path>` with `answers` in order; the last
    /// one repeats.
    pub(crate) fn route(self, method: &'static str, path: &str, answers: Vec<Answer>) -> Self {
        lock(&self.shared.rules).push(Rule {
            method,
            path: format!("/api/v1{path}"),
            answers: answers.into(),
        });
        self
    }

    /// Everything seen so far.
    pub(crate) fn seen(&self) -> Vec<Seen> {
        lock(&self.shared.seen).clone()
    }

    /// The requests seen for `method` and `path` (without `/api/v1`).
    pub(crate) fn seen_at(&self, method: &str, path: &str) -> Vec<Seen> {
        let full = format!("/api/v1{path}");
        self.seen()
            .into_iter()
            .filter(|seen| seen.method == method && seen.path == full)
            .collect()
    }

    /// `GET path` through the fake daemon's tunnel of `environment`
    /// ([`Stub::start_with_tunnel`]): the status and the body the dev
    /// command answered.
    pub(crate) async fn through_tunnel(&self, environment: &str, path: &str) -> (u16, String) {
        let tunnel = self.shared.tunnel.as_ref().expect("a stub with a tunnel");
        let request = axum::http::Request::get(path)
            .header(header::HOST, format!("{environment}.west.test"))
            .body(Body::empty())
            .unwrap();
        let exchange = async {
            let response = tunnel.server.forward(environment, request).await;
            let status = response.status().as_u16();
            let body = axum::body::to_bytes(Body::new(response.into_body()), 1 << 20)
                .await
                .unwrap();
            (status, String::from_utf8_lossy(&body).into_owned())
        };
        tokio::time::timeout(TUNNEL_PATIENCE, exchange)
            .await
            .expect("the dev command answers through the tunnel in time")
    }

    /// `(method, path without /api/v1)` of everything seen, in order.
    pub(crate) fn trail(&self) -> Vec<(String, String)> {
        self.seen()
            .into_iter()
            .map(|seen| {
                let path = seen
                    .path
                    .strip_prefix("/api/v1")
                    .unwrap_or(&seen.path)
                    .to_owned();
                (seen.method, path)
            })
            .collect()
    }
}

async fn handle(State(shared): State<Arc<Shared>>, request: Request) -> Response {
    if request.uri().path() == TUNNEL_PATH
        && let Some(tunnel) = shared.tunnel.clone()
    {
        return tunnel.upgrade(&shared, request);
    }
    let (parts, body) = request.into_parts();
    let bytes = axum::body::to_bytes(body, 1 << 20).await.unwrap();
    let content_type = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let mut fields = Vec::new();
    let mut parts_seen = Vec::new();
    let mut filenames = Vec::new();
    if content_type
        .as_deref()
        .is_some_and(|value| value.starts_with("multipart/form-data"))
    {
        let rebuilt = Request::from_parts(parts.clone(), Body::from(bytes.clone()));
        let mut multipart = Multipart::from_request(rebuilt, &()).await.unwrap();
        while let Some(field) = multipart.next_field().await.unwrap() {
            let name = field.name().unwrap_or_default().to_owned();
            filenames.push(field.file_name().map(str::to_owned));
            let bytes = field.bytes().await.unwrap().to_vec();
            fields.push((name.clone(), String::from_utf8_lossy(&bytes).into_owned()));
            parts_seen.push((name, bytes));
        }
    }
    let seen = Seen {
        method: parts.method.to_string(),
        path: parts.uri.path().to_owned(),
        query: parts.uri.query().unwrap_or_default().to_owned(),
        authorization: parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned),
        body: String::from_utf8_lossy(&bytes).into_owned(),
        fields,
        parts: parts_seen,
        filenames,
        project: parts
            .headers
            .get(PROJECT_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned),
        status: 404,
    };
    let answer = lock(&shared.rules)
        .iter_mut()
        .find(|rule| rule.method == seen.method && rule.path == seen.path)
        .and_then(Rule::next);
    let mut seen = seen;
    seen.status = answer.as_ref().map_or(404, |answer| answer.status);
    lock(&shared.seen).push(seen);
    match answer {
        None => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .header(header::CONTENT_TYPE, "application/problem+json")
            .body(Body::from(
                r#"{"title":"Not Found","status":404,"detail":"no such route in the stub"}"#,
            ))
            .unwrap(),
        Some(answer) => Response::builder()
            .status(answer.status)
            .header(header::CONTENT_TYPE, answer.content_type)
            .body(Body::from(answer.body))
            .unwrap(),
    }
}

/// The path of the tunnel's upgrade.
const TUNNEL_PATH: &str = "/api/v1/tunnel";

/// The header the CLI names its project in.
const PROJECT_HEADER: &str = "x-liftoff-project";

/// The one-time link the fake daemon answers the upgrade of a private
/// tunnel with (`X-Liftoff-Login-Url`, DEVT-07).
pub(crate) const LOGIN_URL: &str = "https://west.test/api/v1/auth/links/otl_0123456789abcdef";

/// How long a test waits for the next event of the fake tunnel server.
const TUNNEL_PATIENCE: Duration = Duration::from_secs(20);

/// How the fake daemon's `GET /tunnel` behaves.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct TunnelOptions {
    /// The fake drops the first connection as soon as it is the tunnel
    /// (the network broke): the client must come back.
    pub(crate) drop_first: bool,
    /// The environment the fake names the tunnel (`Hello`); `None`:
    /// `<project>-dev`, as single-user mode does.
    pub(crate) environment: Option<&'static str>,
}

/// What the fake tunnel server saw of a connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TunnelEvent {
    /// Both `Hello`s were exchanged: the connection is the tunnel of
    /// `environment`.
    Connected {
        /// The environment the server named.
        environment: String,
    },
    /// The connection ended as the server tells (`Err`: the handshake
    /// failed).
    Ended(Result<Disconnect, String>),
    /// The fake dropped the connection itself ([`TunnelOptions::drop_first`]).
    Dropped,
}

/// The events of the fake tunnel server, in order.
#[derive(Debug)]
pub(crate) struct TunnelEvents(tokio::sync::mpsc::UnboundedReceiver<TunnelEvent>);

impl TunnelEvents {
    /// The next event; `None` when none came within the patience of the
    /// test.
    pub(crate) async fn next(&mut self) -> Option<TunnelEvent> {
        tokio::time::timeout(TUNNEL_PATIENCE, self.0.recv())
            .await
            .ok()
            .flatten()
    }

    /// Waits for the next [`TunnelEvent::Connected`]; whether it came.
    pub(crate) async fn connected(&mut self) -> bool {
        while let Some(event) = self.next().await {
            if matches!(event, TunnelEvent::Connected { .. }) {
                return true;
            }
        }
        false
    }

    /// The next [`TunnelEvent::Ended`] or [`TunnelEvent::Dropped`].
    pub(crate) async fn ended(&mut self) -> Option<TunnelEvent> {
        while let Some(event) = self.next().await {
            if !matches!(event, TunnelEvent::Connected { .. }) {
                return Some(event);
            }
        }
        None
    }
}

/// The daemon's `GET /tunnel`, faked: `401` without the session of
/// [`Cli::logged_in`], else the upgrade to the tunnel of
/// `<project>-dev` (single-user mode; or [`TunnelOptions::environment`]) run by the real [`TunnelServer`]
/// under that name. The `101` carries `X-Liftoff-Login-Url`
/// ([`LOGIN_URL`]) unless a `POST .../<env>/share` the stub answered
/// with a success came before it.
#[derive(Debug)]
pub(crate) struct FakeTunnel {
    server: TunnelServer,
    options: TunnelOptions,
    events: tokio::sync::mpsc::UnboundedSender<TunnelEvent>,
    connections: AtomicUsize,
}

impl FakeTunnel {
    fn upgrade(self: &Arc<Self>, shared: &Shared, mut request: Request) -> Response {
        let header = |name: &str| {
            request
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        };
        let authorization = header(header::AUTHORIZATION.as_str());
        let project = header(PROJECT_HEADER);
        let key = header(header::SEC_WEBSOCKET_KEY.as_str());
        let status = if authorization.as_deref() != Some(&format!("Bearer {SESSION}")) {
            401
        } else if project.is_none() || key.is_none() {
            400
        } else {
            101
        };
        lock(&shared.seen).push(Seen {
            method: request.method().to_string(),
            path: request.uri().path().to_owned(),
            query: request.uri().query().unwrap_or_default().to_owned(),
            authorization,
            body: String::new(),
            fields: Vec::new(),
            parts: Vec::new(),
            filenames: Vec::new(),
            project: project.clone(),
            status,
        });
        let (Some(project), Some(key), 101) = (project, key, status) else {
            return Response::builder()
                .status(status)
                .body(Body::empty())
                .unwrap();
        };
        let environment = self
            .options
            .environment
            .map_or_else(|| format!("{project}-dev"), str::to_owned);
        let visibility = format!("/api/v1/projects/{project}/environments/{environment}/");
        let shared_env = lock(&shared.seen)
            .iter()
            .filter(|seen| seen.method == "POST" && (200..300).contains(&seen.status))
            .filter_map(|seen| seen.path.strip_prefix(&visibility))
            .next_back()
            == Some("share");
        let on_upgrade = hyper::upgrade::on(&mut request);
        drop(tokio::spawn(
            Arc::clone(self).serve(on_upgrade, environment),
        ));
        let mut answer = Response::builder()
            .status(StatusCode::SWITCHING_PROTOCOLS)
            .header(header::UPGRADE, "websocket")
            .header(header::CONNECTION, "upgrade")
            .header(
                header::SEC_WEBSOCKET_ACCEPT,
                derive_accept_key(key.as_bytes()),
            );
        if !shared_env {
            answer = answer.header("x-liftoff-login-url", LOGIN_URL);
        }
        answer.body(Body::empty()).unwrap()
    }

    /// Runs the upgraded connection as the tunnel of `environment` and
    /// tells how it went.
    async fn serve(self: Arc<Self>, on_upgrade: hyper::upgrade::OnUpgrade, environment: String) {
        let tunnel = self;
        let Ok(upgraded) = on_upgrade.await else {
            return;
        };
        let first = tunnel.connections.fetch_add(1, Ordering::SeqCst) == 0;
        let drop_now = tunnel.options.drop_first && first;
        let (fire, fired) = tokio::sync::oneshot::channel::<()>();
        let events = tunnel.events.clone();
        let named = environment.clone();
        let serve = tunnel.server.serve_as(
            &environment,
            &environment,
            hyper_util::rt::TokioIo::new(upgraded),
            move |_registration| {
                drop(events.send(TunnelEvent::Connected { environment: named }));
                if drop_now {
                    let _sent: Result<(), ()> = fire.send(());
                }
            },
        );
        let dropped = async {
            if fired.await.is_err() {
                std::future::pending::<()>().await;
            }
        };
        let event = tokio::select! {
            end = serve => TunnelEvent::Ended(end.map_err(|error| error.to_string())),
            () = dropped => TunnelEvent::Dropped,
        };
        drop(tunnel.events.send(event));
    }
}

/// A buffer the CLI writes to and the test reads from.
#[derive(Debug, Clone, Default)]
pub(crate) struct Buf(Arc<Mutex<Vec<u8>>>);

impl Buf {
    pub(crate) fn text(&self) -> String {
        String::from_utf8_lossy(&lock(&self.0)).into_owned()
    }
}

impl Write for Buf {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        lock(&self.0).extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A clock that does not wait, and remembers every pause asked of it.
#[derive(Debug, Clone, Default)]
pub(crate) struct FakeSleeper(Arc<Mutex<Vec<Duration>>>);

impl Sleeper for FakeSleeper {
    async fn sleep(&self, duration: Duration) {
        lock(&self.0).push(duration);
        tokio::task::yield_now().await;
    }
}

/// One scripted answer of the user at the terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Reply {
    /// Typed text (also answers a password question).
    Text(String),
    /// Yes to a confirmation.
    Yes,
    /// No to a confirmation.
    No,
    /// The option at this index of a list.
    Pick(usize),
}

/// The user types `text`.
pub(crate) fn say(text: &str) -> Reply {
    Reply::Text(text.to_owned())
}

#[derive(Debug, Default)]
struct ScriptState {
    replies: VecDeque<Reply>,
    asked: Vec<String>,
    /// The questions asked without echo (`password`).
    hidden: Vec<String>,
}

/// A user at the terminal who answers from a script and remembers every
/// question. Out of answers means the user pressed Esc.
#[derive(Debug, Clone, Default)]
pub(crate) struct Script(Arc<Mutex<ScriptState>>);

impl Script {
    fn next(&self, question: &str) -> Result<Reply, PromptError> {
        let mut state = lock(&self.0);
        state.asked.push(question.to_owned());
        state.replies.pop_front().ok_or(PromptError::Cancelled)
    }

    fn mismatch(question: &str, reply: &Reply) -> PromptError {
        PromptError::Terminal(format!("script answered {reply:?} to {question:?}"))
    }
}

impl Prompter for Script {
    fn text(&mut self, question: &str) -> Result<String, PromptError> {
        match self.next(question)? {
            Reply::Text(text) => Ok(text),
            other @ (Reply::Yes | Reply::No | Reply::Pick(_)) => {
                Err(Self::mismatch(question, &other))
            }
        }
    }

    fn password(&mut self, question: &str) -> Result<String, PromptError> {
        lock(&self.0).hidden.push(question.to_owned());
        self.text(question)
    }

    fn confirm(&mut self, question: &str) -> Result<bool, PromptError> {
        match self.next(question)? {
            Reply::Yes => Ok(true),
            Reply::No => Ok(false),
            other @ (Reply::Text(_) | Reply::Pick(_)) => Err(Self::mismatch(question, &other)),
        }
    }

    fn select(&mut self, question: &str, options: &[String]) -> Result<usize, PromptError> {
        let asked = format!("{question} {options:?}");
        match self.next(&asked)? {
            Reply::Pick(index) if index < options.len() => Ok(index),
            other => Err(Self::mismatch(&asked, &other)),
        }
    }
}

/// A working tree whose `origin` is fixed by the test.
#[derive(Debug, Clone)]
struct FixedRemote(Option<String>);

impl GitRemote for FixedRemote {
    fn origin(&self, _dir: &Path) -> Option<String> {
        self.0.clone()
    }
}

/// A program `server upgrade` ran on the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HostRun {
    pub(crate) program: String,
    pub(crate) args: Vec<String>,
    pub(crate) stdin: Option<String>,
}

/// The host: records every program and fails the one named by
/// [`Cli::failing_host`].
#[derive(Debug, Clone, Default)]
struct FakeHost {
    runs: Arc<Mutex<Vec<HostRun>>>,
    fails: Option<String>,
}

impl HostCommand for FakeHost {
    fn run(&mut self, program: &str, args: &[&str], stdin: Option<&str>) -> Result<(), String> {
        lock(&self.runs).push(HostRun {
            program: program.to_owned(),
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
            stdin: stdin.map(str::to_owned),
        });
        match &self.fails {
            Some(failing) if failing == program => {
                Err(format!("`{program}` failed (exit status: 1)"))
            }
            _ => Ok(()),
        }
    }
}

/// What a run printed and returned.
#[derive(Debug)]
pub(crate) struct Outcome {
    pub(crate) code: u8,
    pub(crate) out: String,
    pub(crate) err: String,
}

/// The name under which [`Cli::logged_in`] saves the stub server.
pub(crate) const SERVER: &str = "main";

/// The session token [`Cli::logged_in`] saves.
pub(crate) const SESSION: &str = "lfs_session";

/// One CLI invocation environment: a temp home with the config, a working
/// directory, and the injected effects. Batch mode (no terminal) unless
/// [`Cli::terminal`] says otherwise.
#[derive(Debug)]
pub(crate) struct Cli {
    _dir: tempfile::TempDir,
    pub(crate) config: PathBuf,
    /// The top of the working tree; the working directory unless
    /// [`Cli::inside`] moved it.
    pub(crate) root: PathBuf,
    pub(crate) cwd: PathBuf,
    tty: bool,
    script: Script,
    remote: Option<String>,
    host: FakeHost,
    sleeper: FakeSleeper,
    local: Local,
    /// The Ctrl-C of every run ([`Cli::interrupt`]).
    cancel: CancellationToken,
    /// The stdout and stderr of the latest run, also while it runs
    /// ([`Cli::printed`]).
    live: Mutex<(Buf, Buf)>,
}

/// A module registry nobody serves, under a key nothing is signed with:
/// local mode of a test that sets up none ([`Cli::local`]) installs no
/// module.
fn unreachable_registry() -> Local {
    Local::new("http://127.0.0.1:9/index.json", RegistryKey::new(""))
}

impl Cli {
    /// No config file at all; the working directory is not a repository.
    pub(crate) fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = dir
            .path()
            .join("home")
            .join(".config")
            .join("liftoff")
            .join("config.toml");
        let cwd = dir.path().join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        Self {
            _dir: dir,
            config,
            root: cwd.clone(),
            cwd,
            tty: false,
            script: Script::default(),
            remote: None,
            host: FakeHost::default(),
            sleeper: FakeSleeper::default(),
            local: unreachable_registry(),
            cancel: CancellationToken::new(),
            live: Mutex::default(),
        }
    }

    /// Local mode runs with `local` (its registry, its forge routes, the
    /// environment of its `git` children).
    pub(crate) fn local(mut self, local: Local) -> Self {
        self.local = local;
        self
    }

    /// The config home (`~/.config/liftoff/`): the directory of the
    /// config file, where local mode keeps its files.
    pub(crate) fn config_dir(&self) -> PathBuf {
        self.config.parent().unwrap().to_path_buf()
    }

    /// Deletes the config file: the next run is in local mode again.
    pub(crate) fn forget_config(&self) {
        std::fs::remove_file(&self.config).unwrap();
    }

    /// Logged in to `server` (saved as [`SERVER`], the default) with token
    /// [`SESSION`].
    pub(crate) fn logged_in(server: &str) -> Self {
        Self::logged_in_as(server, SESSION)
    }

    /// What the latest [`Cli::run`] printed so far: stdout and stderr.
    pub(crate) fn printed(&self) -> (String, String) {
        let live = lock(&self.live);
        (live.0.text(), live.1.text())
    }

    /// [`Cli::logged_in`] with the session `token`.
    pub(crate) fn logged_in_as(server: &str, token: &str) -> Self {
        let cli = Self::new();
        cli.write_config(&format!(
            "default_server = \"{SERVER}\"\n\n[servers.{SERVER}]\nurl = \"{server}\"\ntoken = \"{token}\"\n"
        ));
        cli
    }

    /// Writes the config of [`Cli::logged_in`]: `server` saved as
    /// [`SERVER`], the default, with token [`SESSION`].
    pub(crate) fn log_in(&self, server: &str) {
        self.write_config(&format!(
            "default_server = \"{SERVER}\"\n\n[servers.{SERVER}]\nurl = \"{server}\"\ntoken = \"{SESSION}\"\n"
        ));
    }

    /// Replaces the config file.
    pub(crate) fn write_config(&self, content: &str) {
        std::fs::create_dir_all(self.config.parent().unwrap()).unwrap();
        std::fs::write(&self.config, content).unwrap();
    }

    /// The config file as text.
    pub(crate) fn config_text(&self) -> String {
        std::fs::read_to_string(&self.config).unwrap_or_default()
    }

    /// The config file as a TOML table.
    pub(crate) fn config_toml(&self) -> toml::Table {
        self.config_text().parse().unwrap()
    }

    /// The working directory is a repository whose `origin` is `url`.
    pub(crate) fn in_repo(mut self, url: &str) -> Self {
        self.remote = Some(url.to_owned());
        self
    }

    /// The top of the working tree holds `.git`, and the run starts in its
    /// subdirectory `sub`.
    pub(crate) fn inside(mut self, sub: &str) -> Self {
        std::fs::create_dir_all(self.root.join(".git")).unwrap();
        self.cwd = self.root.join(sub);
        std::fs::create_dir_all(&self.cwd).unwrap();
        self
    }

    /// Writes `name` at the top of the working tree.
    pub(crate) fn write_root(&self, name: &str, content: &str) {
        std::fs::write(self.root.join(name), content).unwrap();
    }

    /// A file at the top of the working tree, if it exists.
    pub(crate) fn root_text(&self, name: &str) -> Option<String> {
        std::fs::read_to_string(self.root.join(name)).ok()
    }

    /// Writes `.liftoff` in the working directory.
    pub(crate) fn dotfile(&self, content: &str) {
        std::fs::write(self.cwd.join(".liftoff"), content).unwrap();
    }

    /// A file of the working directory, if it exists.
    pub(crate) fn cwd_text(&self, name: &str) -> Option<String> {
        std::fs::read_to_string(self.cwd.join(name)).ok()
    }

    /// Stdin is a terminal whose user answers `replies` in order.
    pub(crate) fn terminal(mut self, replies: Vec<Reply>) -> Self {
        self.tty = true;
        lock(&self.script.0).replies = replies.into();
        self
    }

    /// The host fails `program`.
    pub(crate) fn failing_host(mut self, program: &str) -> Self {
        self.host.fails = Some(program.to_owned());
        self
    }

    /// Every program run on the host, in order.
    pub(crate) fn host_runs(&self) -> Vec<HostRun> {
        lock(&self.host.runs).clone()
    }

    /// Ctrl-C: the run in progress (and every later one) is cancelled.
    pub(crate) fn interrupt(&self) {
        self.cancel.cancel();
    }

    /// Every pause the runs asked for, in order.
    pub(crate) fn slept(&self) -> Vec<Duration> {
        lock(&self.sleeper.0).clone()
    }

    /// Every question the run asked, in order.
    pub(crate) fn asked(&self) -> Vec<String> {
        lock(&self.script.0).asked.clone()
    }

    /// The questions the run asked without echo, in order.
    pub(crate) fn asked_hidden(&self) -> Vec<String> {
        lock(&self.script.0).hidden.clone()
    }

    /// The environment of one run, writing to `out` and `err`.
    fn env(&self, out: &Buf, err: &Buf) -> Env<FakeSleeper> {
        Env {
            config_path: self.config.clone(),
            cwd: self.cwd.clone(),
            out: Box::new(out.clone()),
            err: Box::new(err.clone()),
            tty: self.tty,
            prompter: Box::new(self.script.clone()),
            git: Box::new(FixedRemote(self.remote.clone())),
            host: Box::new(self.host.clone()),
            machine: Some("test-box".to_owned()),
            sleeper: self.sleeper.clone(),
            cancel: self.cancel.clone(),
            local: self.local.clone(),
        }
    }

    /// Runs `liftoff <args>`.
    pub(crate) async fn run(&self, args: &[&str]) -> Outcome {
        let words = std::iter::once("liftoff")
            .chain(args.iter().copied())
            .map(str::to_owned);
        // Each run prints into buffers of its own: a test that runs the
        // CLI twice sees what the second run printed.
        let (out, err) = (Buf::default(), Buf::default());
        *lock(&self.live) = (out.clone(), err.clone());
        let code = run(words, &mut self.env(&out, &err)).await;
        Outcome {
            code,
            out: out.text(),
            err: err.text(),
        }
    }
}
