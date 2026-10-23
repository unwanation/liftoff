//! [`RecordingDocker`] and [`RecordingFiles`].

use core::future::ready;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};
use core::time::Duration;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use futures_core::Stream;

use super::log::{Call, CallLog, DockerCall, Faults, FilesCall, lock, scriptable};
use crate::domain::{ComposeProject, Host, Login, ServiceName};
use crate::error::Error;
use crate::ports::{CommandOutput, Docker, ProjectFile, ProjectFiles, ServiceStatus};

/// A stream over lines already in memory.
#[derive(Debug)]
pub struct LineStream {
    lines: std::vec::IntoIter<String>,
    /// A followed stream stays pending when the lines run out, like
    /// `docker compose logs -f` on a quiet service.
    follow: bool,
    _open: OpenStream,
}

/// Counts a log stream as open until it is dropped.
#[derive(Debug)]
struct OpenStream(Arc<AtomicUsize>);

impl OpenStream {
    fn start(counter: &Arc<AtomicUsize>) -> Self {
        let _previous = counter.fetch_add(1, Ordering::SeqCst);
        Self(Arc::clone(counter))
    }
}

impl Drop for OpenStream {
    fn drop(&mut self) {
        let _previous = self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Stream for LineStream {
    type Item = Result<String, Error>;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match this.lines.next() {
            Some(line) => Poll::Ready(Some(Ok(line))),
            None if this.follow => Poll::Pending,
            None => Poll::Ready(None),
        }
    }
}

#[derive(Debug, Default)]
struct GateState {
    held: bool,
    waiting: Vec<Waker>,
}

/// Holds back the Docker commands of one op until [`Gate::release`]
/// (see [`RecordingDocker::hold`]).
///
/// Clones share the gate. Dropping it does **not** release the calls.
#[derive(Debug, Clone, Default)]
pub struct Gate(Arc<Mutex<GateState>>);

impl Gate {
    pub(super) fn held() -> Self {
        Self(Arc::new(Mutex::new(GateState {
            held: true,
            waiting: Vec::new(),
        })))
    }

    /// Lets every held call finish, and every later call pass.
    pub fn release(&self) {
        let waiting = {
            let mut state = lock(&self.0);
            state.held = false;
            core::mem::take(&mut state.waiting)
        };
        for waker in waiting {
            waker.wake();
        }
    }

    /// Whether calls are still held.
    #[must_use]
    pub fn is_held(&self) -> bool {
        lock(&self.0).held
    }

    /// `true` and registers `waker` while held.
    pub(super) fn wait(&self, waker: &Waker) -> bool {
        let mut state = lock(&self.0);
        if state.held && !state.waiting.iter().any(|w| w.will_wake(waker)) {
            state.waiting.push(waker.clone());
        }
        state.held
    }
}

#[derive(Debug, Default)]
struct Flights {
    now: usize,
    max: usize,
}

/// Counts one command as in flight from the call until its future
/// completes or is dropped.
#[derive(Debug)]
struct Flight(Arc<Mutex<Flights>>);

impl Flight {
    fn start(flights: &Arc<Mutex<Flights>>) -> Self {
        {
            let mut counts = lock(flights);
            counts.now = counts.now.saturating_add(1);
            counts.max = counts.max.max(counts.now);
        }
        Self(Arc::clone(flights))
    }
}

impl Drop for Flight {
    fn drop(&mut self) {
        let mut counts = lock(&self.0);
        counts.now = counts.now.saturating_sub(1);
    }
}

/// The future of a [`RecordingDocker`] command: waits while its op is held
/// by a [`Gate`], panics when [`RecordingDocker::panic_next`] scripted it,
/// then yields the scripted result.
#[derive(Debug)]
pub struct DockerReply {
    gate: Option<Gate>,
    panics: bool,
    result: Option<Result<CommandOutput, Error>>,
    flight: Option<Flight>,
}

impl Future for DockerReply {
    type Output = Result<CommandOutput, Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if this.gate.as_ref().is_some_and(|gate| gate.wait(cx.waker())) {
            return Poll::Pending;
        }
        assert!(!this.panics, "RecordingDocker: scripted panic");
        this.flight = None;
        Poll::Ready(
            this.result
                .take()
                .unwrap_or_else(|| Err(Error::internal_msg("DockerReply polled after completion"))),
        )
    }
}

#[derive(Debug, Default)]
struct DockerState {
    outputs: HashMap<&'static str, VecDeque<CommandOutput>>,
    project_outputs: HashMap<(ComposeProject, &'static str), VecDeque<CommandOutput>>,
    gates: HashMap<&'static str, Gate>,
    panics: HashMap<&'static str, usize>,
    ps: HashMap<ComposeProject, Vec<ServiceStatus>>,
    logs: HashMap<ComposeProject, Vec<String>>,
}

impl DockerState {
    fn next_output(&mut self, op: &'static str, project: Option<&ComposeProject>) -> CommandOutput {
        project
            .and_then(|p| {
                self.project_outputs
                    .get_mut(&(p.clone(), op))
                    .and_then(VecDeque::pop_front)
            })
            .or_else(|| self.outputs.get_mut(op).and_then(VecDeque::pop_front))
            .unwrap_or_else(|| CommandOutput::ok([format!("{op} ok").as_str()]))
    }

    fn take_panic(&mut self, op: &'static str) -> bool {
        self.panics.get_mut(op).is_some_and(|left| {
            let armed = *left > 0;
            *left = left.saturating_sub(1);
            armed
        })
    }
}

/// Records [`Docker`] calls and answers from a script.
///
/// - Every command succeeds with one stdout line (`"<op> ok"`) unless
///   [`RecordingDocker::respond_next`] queued an output for that op, e.g. a
///   failed `compose_up` with stderr lines (DEP-09, DEP-10).
/// - `compose_ps` answers what [`RecordingDocker::set_ps`] set (empty by
///   default).
/// - `logs` streams what [`RecordingDocker::set_logs`] set; with `follow`
///   it then stays open ([`RecordingDocker::open_log_streams`]).
/// - [`RecordingDocker::fail_next`] makes an op return `Err` (the command
///   could not run at all).
/// - [`RecordingDocker::hold`] keeps the commands of an op pending until
///   the returned [`Gate`] is released, so a test can watch a deploy while
///   it is `running` (QUE-01, QUE-04); [`RecordingDocker::in_flight`] and
///   [`RecordingDocker::max_in_flight`] count overlapping commands
///   (QUE-02); [`RecordingDocker::panic_next`] makes one command panic
///   (QUE-03).
///
/// The commands (`login`, `compose_pull`, `compose_up`, `compose_down`)
/// are logged, take their scripted output and fault when called, and
/// complete when polled with their op not held.
#[derive(Debug, Clone, Default)]
pub struct RecordingDocker {
    state: Arc<Mutex<DockerState>>,
    flights: Arc<Mutex<Flights>>,
    open_logs: Arc<AtomicUsize>,
    log: CallLog,
    faults: Faults,
}

scriptable!(RecordingDocker);

impl RecordingDocker {
    /// A fake with its own call log.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A fake writing to `log`.
    #[must_use]
    pub fn with_log(log: CallLog) -> Self {
        Self {
            log,
            ..Self::default()
        }
    }

    /// The next call of `op` (`"login"`, `"compose_pull"`, `"compose_up"`,
    /// `"compose_down"`) returns `output`.
    pub fn respond_next(&self, op: &'static str, output: CommandOutput) {
        lock(&self.state)
            .outputs
            .entry(op)
            .or_default()
            .push_back(output);
    }

    /// The next call of `op` for `project` returns `output`; takes
    /// precedence over [`RecordingDocker::respond_next`], so scripted
    /// outputs reach the intended environment when deploys run in parallel.
    pub fn respond_next_for(
        &self,
        project: &ComposeProject,
        op: &'static str,
        output: CommandOutput,
    ) {
        lock(&self.state)
            .project_outputs
            .entry((project.clone(), op))
            .or_default()
            .push_back(output);
    }

    /// Holds every call of `op` made from now on until the returned gate is
    /// released. Holding an op again replaces
    /// its gate; release the old one first.
    #[must_use = "the calls stay pending until the gate is released"]
    pub fn hold(&self, op: &'static str) -> Gate {
        let gate = Gate::held();
        let _previous: Option<Gate> = lock(&self.state).gates.insert(op, gate.clone());
        gate
    }

    /// The next call of `op` panics when polled (QUE-03).
    pub fn panic_next(&self, op: &'static str) {
        let mut state = lock(&self.state);
        let left = state.panics.entry(op).or_default();
        *left = left.saturating_add(1);
        drop(state);
    }

    /// Commands called and not yet finished (held ones included).
    #[must_use]
    pub fn in_flight(&self) -> usize {
        lock(&self.flights).now
    }

    /// The most commands that were in flight at once so far.
    #[must_use]
    pub fn max_in_flight(&self) -> usize {
        lock(&self.flights).max
    }

    /// Shorthand: the next `compose_up` exits 1 after printing `stderr`.
    pub fn fail_next_up<'a>(&self, stderr: impl IntoIterator<Item = &'a str>) {
        self.respond_next("compose_up", CommandOutput::failed(1, stderr));
    }

    /// What `compose_ps` reports for `project` from now on.
    pub fn set_ps(&self, project: &ComposeProject, services: Vec<ServiceStatus>) {
        let _previous: Option<Vec<ServiceStatus>> =
            lock(&self.state).ps.insert(project.clone(), services);
    }

    /// What `logs` streams for `project` from now on.
    pub fn set_logs(&self, project: &ComposeProject, lines: Vec<String>) {
        let _previous: Option<Vec<String>> = lock(&self.state).logs.insert(project.clone(), lines);
    }

    /// How many streams returned by `logs` are still alive (not dropped).
    /// A followed stream never ends by itself, so a test can see the
    /// consumer let go of it.
    #[must_use]
    pub fn open_log_streams(&self) -> usize {
        self.open_logs.load(Ordering::SeqCst)
    }

    /// The Docker calls received so far, oldest first.
    #[must_use]
    pub fn calls(&self) -> Vec<DockerCall> {
        self.log
            .calls()
            .into_iter()
            .filter_map(|call| match call {
                Call::Docker(call) => Some(call),
                _ => None,
            })
            .collect()
    }

    fn command(
        &self,
        op: &'static str,
        project: Option<&ComposeProject>,
        call: DockerCall,
    ) -> DockerReply {
        self.log.push(Call::Docker(call));
        let flight = Flight::start(&self.flights);
        let fault = self.faults.check(op);
        let mut state = lock(&self.state);
        let result = fault.map(|()| state.next_output(op, project));
        if op == "compose_down"
            && let (Some(project), Ok(output)) = (project, &result)
            && output.success()
        {
            // A project that is down has no containers.
            let _gone: Option<Vec<ServiceStatus>> = state.ps.remove(project);
        }
        DockerReply {
            gate: state.gates.get(op).filter(|gate| gate.is_held()).cloned(),
            panics: state.take_panic(op),
            result: Some(result),
            flight: Some(flight),
        }
    }
}

impl Docker for RecordingDocker {
    fn login(
        &self,
        registry: &Host,
        user: &Login,
        _token: &str,
    ) -> impl Future<Output = Result<CommandOutput, Error>> + Send {
        self.command(
            "login",
            None,
            DockerCall::Login {
                registry: registry.clone(),
                user: user.clone(),
            },
        )
    }

    fn compose_pull(
        &self,
        project: &ComposeProject,
    ) -> impl Future<Output = Result<CommandOutput, Error>> + Send {
        self.command(
            "compose_pull",
            Some(project),
            DockerCall::Pull(project.clone()),
        )
    }

    fn compose_up(
        &self,
        project: &ComposeProject,
        wait_timeout: Duration,
    ) -> impl Future<Output = Result<CommandOutput, Error>> + Send {
        self.command(
            "compose_up",
            Some(project),
            DockerCall::Up {
                project: project.clone(),
                wait_timeout,
            },
        )
    }

    fn compose_down(
        &self,
        project: &ComposeProject,
        volumes: bool,
    ) -> impl Future<Output = Result<CommandOutput, Error>> + Send {
        self.command(
            "compose_down",
            Some(project),
            DockerCall::Down {
                project: project.clone(),
                volumes,
            },
        )
    }

    fn compose_ps(
        &self,
        project: &ComposeProject,
    ) -> impl Future<Output = Result<Vec<ServiceStatus>, Error>> + Send {
        self.log.push(Call::Docker(DockerCall::Ps(project.clone())));
        ready(self.faults.check("compose_ps").map(|()| {
            lock(&self.state)
                .ps
                .get(project)
                .cloned()
                .unwrap_or_default()
        }))
    }

    fn logs(
        &self,
        project: &ComposeProject,
        service: Option<&ServiceName>,
        tail: Option<u32>,
        follow: bool,
    ) -> impl Future<
        Output = Result<impl Stream<Item = Result<String, Error>> + Send + 'static, Error>,
    > + Send {
        self.log.push(Call::Docker(DockerCall::Logs {
            project: project.clone(),
            service: service.cloned(),
            tail,
            follow,
        }));
        ready(self.faults.check("logs").map(|()| {
            let lines = lock(&self.state)
                .logs
                .get(project)
                .cloned()
                .unwrap_or_default();
            LineStream {
                lines: lines.into_iter(),
                follow,
                _open: OpenStream::start(&self.open_logs),
            }
        }))
    }
}

type FileMap = BTreeMap<(ComposeProject, ProjectFile), String>;

/// Keeps compose project files in memory and records writes.
///
/// [`RecordingFiles::file`] reads back the current content; writes and
/// removals are logged as [`Call::Files`].
#[derive(Debug, Clone, Default)]
pub struct RecordingFiles {
    files: Arc<Mutex<FileMap>>,
    log: CallLog,
    faults: Faults,
}

scriptable!(RecordingFiles);

impl RecordingFiles {
    /// A fake with its own call log.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A fake writing to `log`.
    #[must_use]
    pub fn with_log(log: CallLog) -> Self {
        Self {
            log,
            ..Self::default()
        }
    }

    /// The current content of a file.
    #[must_use]
    pub fn file(&self, project: &ComposeProject, file: ProjectFile) -> Option<String> {
        lock(&self.files).get(&(project.clone(), file)).cloned()
    }

    /// Whether any file of `project` exists.
    #[must_use]
    pub fn has_project(&self, project: &ComposeProject) -> bool {
        lock(&self.files).keys().any(|(p, _)| p == project)
    }
}

impl ProjectFiles for RecordingFiles {
    fn write(
        &self,
        project: &ComposeProject,
        file: ProjectFile,
        content: &str,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        self.log.push(Call::Files(FilesCall::Write {
            project: project.clone(),
            file,
            content: content.to_owned(),
        }));
        ready(self.faults.check("write").map(|()| {
            let _previous: Option<String> =
                lock(&self.files).insert((project.clone(), file), content.to_owned());
        }))
    }

    fn read(
        &self,
        project: &ComposeProject,
        file: ProjectFile,
    ) -> impl Future<Output = Result<Option<String>, Error>> + Send {
        ready(self.faults.check("read").map(|()| self.file(project, file)))
    }

    fn remove_all(
        &self,
        project: &ComposeProject,
    ) -> impl Future<Output = Result<(), Error>> + Send {
        self.log
            .push(Call::Files(FilesCall::RemoveAll(project.clone())));
        ready(self.faults.check("remove_all").map(|()| {
            lock(&self.files).retain(|(p, _), _| p != project);
        }))
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;
    use std::panic::{AssertUnwindSafe, catch_unwind};

    use super::*;
    use crate::domain::{EnvName, ProjectName};

    fn project(env: &str) -> ComposeProject {
        ComposeProject::new(
            &ProjectName::new("acme").unwrap(),
            &EnvName::new(env).unwrap(),
        )
    }

    fn poll_once(reply: &mut DockerReply) -> Poll<Result<CommandOutput, Error>> {
        Pin::new(reply).poll(&mut Context::from_waker(Waker::noop()))
    }

    fn up(docker: &RecordingDocker, env: &str) -> DockerReply {
        docker.command(
            "compose_up",
            Some(&project(env)),
            DockerCall::Up {
                project: project(env),
                wait_timeout: Duration::from_secs(1),
            },
        )
    }

    mod hold {
        use super::*;

        // TC: v1 QUE-01, v1 QUE-04 (a deploy can be kept running)
        #[test]
        fn pending_until_released_when_op_held() {
            let docker = RecordingDocker::new();
            let gate = docker.hold("compose_up");
            let mut reply = up(&docker, "qa");
            assert!(poll_once(&mut reply).is_pending(), "held");
            gate.release();
            assert!(
                matches!(poll_once(&mut reply), Poll::Ready(Ok(out)) if out.success()),
                "released"
            );
        }

        #[test]
        fn other_ops_pass_when_one_op_held() {
            let docker = RecordingDocker::new();
            let _gate = docker.hold("compose_pull");
            assert!(poll_once(&mut up(&docker, "qa")).is_ready(), "not held");
        }
    }

    mod in_flight {
        use super::*;

        // TC: v1 QUE-02 (overlap is observable)
        #[test]
        fn counts_peak_when_commands_overlap() {
            let docker = RecordingDocker::new();
            let gate = docker.hold("compose_up");
            let (mut a, mut b) = (up(&docker, "qa"), up(&docker, "pre"));
            assert_eq!(docker.in_flight(), 2);
            gate.release();
            let _done = (poll_once(&mut a), poll_once(&mut b));
            assert_eq!((docker.in_flight(), docker.max_in_flight()), (0, 2));
        }

        #[test]
        fn drops_count_when_reply_dropped_unfinished() {
            let docker = RecordingDocker::new();
            let _gate = docker.hold("compose_up");
            drop(up(&docker, "qa"));
            assert_eq!(docker.in_flight(), 0);
        }
    }

    mod panic_next {
        use super::*;

        // TC: v1 QUE-03
        #[test]
        fn panics_once_when_scripted() {
            let docker = RecordingDocker::new();
            docker.panic_next("compose_up");
            let mut first = up(&docker, "qa");
            let panicked = catch_unwind(AssertUnwindSafe(|| poll_once(&mut first))).is_err();
            assert!(panicked, "first call panics");
            assert!(
                poll_once(&mut up(&docker, "qa")).is_ready(),
                "second passes"
            );
        }
    }

    mod respond_next_for {
        use super::*;

        #[test]
        fn reaches_only_that_project_when_scripted() {
            let docker = RecordingDocker::new();
            docker.respond_next_for(
                &project("pre"),
                "compose_up",
                CommandOutput::failed(1, ["x"]),
            );
            let qa = poll_once(&mut up(&docker, "qa"));
            let pre = poll_once(&mut up(&docker, "pre"));
            assert!(
                matches!((qa, pre), (Poll::Ready(Ok(q)), Poll::Ready(Ok(p))) if q.success() && !p.success()),
                "only pre fails"
            );
        }
    }
}
