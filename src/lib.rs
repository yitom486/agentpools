//! A bounded, reusable session pool for agent-backed tasks.
//!
//! The pool schedules requests and owns session lifetimes. An [`AgentBackend`]
//! owns the protocol, tools, permissions, and the exact session configuration.
//! In particular, an ACP adapter can forward caller-provided MCP servers
//! unchanged when it opens a session; this crate does not parse MCP settings.

use std::collections::VecDeque;
use std::fmt;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread::{self, JoinHandle};

/// The protocol-specific session owned by one worker at a time.
///
/// `run` may contain several prompt or tool interactions. Implementations should use
/// finite I/O timeouts and cooperate with `cancellation` when possible.
pub trait AgentSession<Request, Response, Error>: Send {
    fn run(
        &mut self,
        request: Request,
        cancellation: &CancellationToken,
    ) -> Result<Response, Error>;

    fn close(&mut self) -> Result<(), Error>;

    /// Whether a failed `run` left the same session ready for another request.
    /// Returning `false` is the safe default for broken transports and unknown
    /// protocol state. The pool only retries on this session when it is `true`.
    fn can_retry_after(&self, _error: &Error) -> bool {
        false
    }
}

/// Creates sessions for the pool. `Config` belongs entirely to the adapter.
///
/// The pool stores one immutable config and passes it by reference to every
/// `open` call. An adapter can therefore use a typed config containing MCP
/// servers, tool permissions, credentials, model settings, or other options.
pub trait AgentBackend: Send + Sync + 'static {
    type Config: Send + Sync + 'static;
    type Request: Send + 'static;
    type Response: Send + 'static;
    type Error: Send + 'static;
    type Session: AgentSession<Self::Request, Self::Response, Self::Error> + 'static;

    fn open(&self, config: &Self::Config) -> Result<Self::Session, Self::Error>;
}

/// Cooperative cancellation shared by a caller and the active session.
#[derive(Clone, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// Limits both live sessions and requests waiting to be assigned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolConfig {
    pub workers: usize,
    pub max_queued: usize,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            workers: 4,
            max_queued: 128,
        }
    }
}

#[derive(Debug)]
pub enum BuildError {
    InvalidConfig,
    Spawn(io::Error),
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig => write!(f, "workers and max_queued must be positive"),
            Self::Spawn(error) => write!(f, "cannot start pool worker: {error}"),
        }
    }
}

impl std::error::Error for BuildError {}

/// A rejected submission retains ownership of its request.
#[derive(Debug)]
pub enum SubmitError<Request> {
    Closed(Request),
    QueueFull(Request),
    NoSuchAgent(Request),
}

impl<Request> fmt::Display for SubmitError<Request> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed(_) => write!(f, "agent pool is closed"),
            Self::QueueFull(_) => write!(f, "agent pool queue is full"),
            Self::NoSuchAgent(_) => write!(f, "agent index does not exist"),
        }
    }
}

/// Result of submitting a request to a particular backend pool.
pub type SubmitResult<B> = Result<
    TaskHandle<<B as AgentBackend>::Response, <B as AgentBackend>::Error>,
    SubmitError<<B as AgentBackend>::Request>,
>;

/// A task failure. Failed `run` calls close their session; they are not
/// automatically retried because the request may have produced side effects.
#[derive(Debug)]
pub enum TaskError<Error> {
    Open(Error),
    Run { source: Error, close: Option<Error> },
    Cancelled,
    WorkerStopped,
}

impl<Error: fmt::Display> fmt::Display for TaskError<Error> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Open(error) => write!(f, "cannot open agent session: {error}"),
            Self::Run { source, .. } => write!(f, "agent task failed: {source}"),
            Self::Cancelled => write!(f, "agent task cancelled"),
            Self::WorkerStopped => write!(f, "agent worker stopped"),
        }
    }
}

impl<Error: std::error::Error + 'static> std::error::Error for TaskError<Error> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Open(error) | Self::Run { source: error, .. } => Some(error),
            Self::Cancelled | Self::WorkerStopped => None,
        }
    }
}

/// One submitted task. Waiting on handles in submission order restores input
/// order even when agents complete in a different order.
pub struct TaskHandle<Response, Error> {
    id: u64,
    cancellation: CancellationToken,
    result: mpsc::Receiver<Result<Response, TaskError<Error>>>,
}

impl<Response, Error> TaskHandle<Response, Error> {
    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn cancel(&self) {
        self.cancellation.cancel();
    }

    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    pub fn wait(self) -> Result<Response, TaskError<Error>> {
        self.result.recv().unwrap_or(Err(TaskError::WorkerStopped))
    }
}

/// Wait for every task and return one result per input handle, in input order.
pub fn collect_ordered<Response, Error>(
    handles: impl IntoIterator<Item = TaskHandle<Response, Error>>,
) -> Vec<Result<Response, TaskError<Error>>> {
    handles.into_iter().map(TaskHandle::wait).collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownMode {
    /// Finish queued requests before closing sessions.
    Drain,
    /// Reject queued requests; running requests are allowed to finish.
    CancelPending,
}

/// Session close errors are preserved for the caller; a panicked worker is
/// counted separately. A protocol adapter should enforce finite I/O timeouts.
pub struct ShutdownReport<Error> {
    pub close_errors: Vec<Error>,
    pub panicked_workers: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolStatus {
    pub queued: usize,
    pub active: usize,
    pub closed: bool,
}

struct Job<B: AgentBackend> {
    request: B::Request,
    retry: Option<RetryPlan<B>>,
    target: Option<usize>,
    cancellation: CancellationToken,
    result: mpsc::Sender<Result<B::Response, TaskError<B::Error>>>,
}

struct RetryPlan<B: AgentBackend> {
    max_attempts: std::num::NonZeroUsize,
    next_request: Box<NextRequest<B>>,
}

type NextRequest<B> =
    dyn FnMut(&<B as AgentBackend>::Error, usize) -> Option<<B as AgentBackend>::Request> + Send;

struct Queue<B: AgentBackend> {
    jobs: VecDeque<Job<B>>,
    closed: bool,
}

struct Shared<B: AgentBackend> {
    queue: Mutex<Queue<B>>,
    ready: Condvar,
    active: AtomicUsize,
    max_queued: usize,
    agent_count: usize,
}

struct WorkerAgent<B: AgentBackend> {
    backend: Arc<B>,
    session_config: Arc<B::Config>,
}

/// A persistent, bounded pool. Sessions start lazily, one per busy worker,
/// and are reused for later tasks assigned to that worker.
pub struct AgentPool<B: AgentBackend> {
    shared: Arc<Shared<B>>,
    workers: Vec<JoinHandle<Option<B::Error>>>,
    next_id: AtomicU64,
}

impl<B: AgentBackend> AgentPool<B> {
    pub fn new(
        backend: B,
        session_config: B::Config,
        config: PoolConfig,
    ) -> Result<Self, BuildError> {
        if config.workers == 0 || config.max_queued == 0 {
            return Err(BuildError::InvalidConfig);
        }
        let backend = Arc::new(backend);
        let session_config = Arc::new(session_config);
        let agents = (0..config.workers)
            .map(|_| WorkerAgent {
                backend: Arc::clone(&backend),
                session_config: Arc::clone(&session_config),
            })
            .collect();
        Self::start(agents, config.max_queued)
    }

    /// Start one worker per supplied Agent and configuration pair. This can
    /// mix profiles or model settings within an adapter. Protocols with
    /// different Rust types can be wrapped by a caller-defined enum adapter.
    pub fn with_agents(agents: Vec<(B, B::Config)>, max_queued: usize) -> Result<Self, BuildError> {
        if agents.is_empty() || max_queued == 0 {
            return Err(BuildError::InvalidConfig);
        }
        let agents = agents
            .into_iter()
            .map(|(backend, session_config)| WorkerAgent {
                backend: Arc::new(backend),
                session_config: Arc::new(session_config),
            })
            .collect();
        Self::start(agents, max_queued)
    }

    fn start(agents: Vec<WorkerAgent<B>>, max_queued: usize) -> Result<Self, BuildError> {
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue {
                jobs: VecDeque::new(),
                closed: false,
            }),
            ready: Condvar::new(),
            active: AtomicUsize::new(0),
            max_queued,
            agent_count: agents.len(),
        });
        let mut pool = Self {
            shared,
            workers: Vec::with_capacity(agents.len()),
            next_id: AtomicU64::new(1),
        };
        for (index, agent) in agents.into_iter().enumerate() {
            let shared = Arc::clone(&pool.shared);
            match thread::Builder::new()
                .name(format!("agentpools-{index}"))
                .spawn(move || worker(shared, agent, index))
            {
                Ok(handle) => pool.workers.push(handle),
                Err(error) => {
                    let _ = pool.shutdown(ShutdownMode::CancelPending);
                    return Err(BuildError::Spawn(error));
                }
            }
        }
        Ok(pool)
    }

    /// Submit a request without blocking. A full queue returns the original
    /// request so the caller can apply its own backpressure policy.
    pub fn submit(&self, request: B::Request) -> SubmitResult<B> {
        self.submit_routed(None, request, None)
    }

    /// Submit to a specific zero-based worker index from `with_agents`.
    /// Use this when a task needs that Agent's profile or tool set.
    pub fn submit_to(&self, agent_index: usize, request: B::Request) -> SubmitResult<B> {
        self.submit_routed(Some(agent_index), request, None)
    }

    /// Keep one worker and its session for the whole task, including recoverable
    /// failures and caller-directed retries. `next_request` receives the error
    /// and the failed attempt number, so it can send feedback to the same Agent.
    /// Returning `None` ends the task. At most `max_attempts` calls are made.
    /// The caller must only retry operations whose side effects are acceptable.
    pub fn submit_retrying(
        &self,
        request: B::Request,
        max_attempts: std::num::NonZeroUsize,
        next_request: impl FnMut(&B::Error, usize) -> Option<B::Request> + Send + 'static,
    ) -> SubmitResult<B> {
        self.submit_routed(
            None,
            request,
            Some(RetryPlan {
                max_attempts,
                next_request: Box::new(next_request),
            }),
        )
    }

    /// Like [`Self::submit_retrying`], but reserves one chosen worker.
    pub fn submit_retrying_to(
        &self,
        agent_index: usize,
        request: B::Request,
        max_attempts: std::num::NonZeroUsize,
        next_request: impl FnMut(&B::Error, usize) -> Option<B::Request> + Send + 'static,
    ) -> SubmitResult<B> {
        self.submit_routed(
            Some(agent_index),
            request,
            Some(RetryPlan {
                max_attempts,
                next_request: Box::new(next_request),
            }),
        )
    }

    fn submit_routed(
        &self,
        target: Option<usize>,
        request: B::Request,
        retry: Option<RetryPlan<B>>,
    ) -> SubmitResult<B> {
        let mut queue = self.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
        if queue.closed {
            return Err(SubmitError::Closed(request));
        }
        if target.is_some_and(|index| index >= self.shared.agent_count) {
            return Err(SubmitError::NoSuchAgent(request));
        }
        if queue.jobs.len() >= self.shared.max_queued {
            return Err(SubmitError::QueueFull(request));
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let cancellation = CancellationToken::default();
        let (sender, result) = mpsc::channel();
        queue.jobs.push_back(Job {
            request,
            retry,
            target,
            cancellation: cancellation.clone(),
            result: sender,
        });
        // Targeted tasks must wake their chosen worker, not an idle sibling.
        self.shared.ready.notify_all();
        Ok(TaskHandle {
            id,
            cancellation,
            result,
        })
    }

    pub fn status(&self) -> PoolStatus {
        let queue = self.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
        PoolStatus {
            queued: queue.jobs.len(),
            active: self.shared.active.load(Ordering::Acquire),
            closed: queue.closed,
        }
    }

    /// Stop accepting tasks, then join workers and close their sessions.
    /// Running calls must finish cooperatively or reach their adapter timeout.
    pub fn shutdown(&mut self, mode: ShutdownMode) -> ShutdownReport<B::Error> {
        {
            let mut queue = self.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
            queue.closed = true;
            if mode == ShutdownMode::CancelPending {
                for job in queue.jobs.drain(..) {
                    job.cancellation.cancel();
                    let _ = job.result.send(Err(TaskError::Cancelled));
                }
            }
            self.shared.ready.notify_all();
        }
        let mut report = ShutdownReport {
            close_errors: Vec::new(),
            panicked_workers: 0,
        };
        for handle in self.workers.drain(..) {
            match handle.join() {
                Ok(Some(error)) => report.close_errors.push(error),
                Ok(None) => {}
                Err(_) => report.panicked_workers += 1,
            }
        }
        report
    }
}

impl<B: AgentBackend> Drop for AgentPool<B> {
    fn drop(&mut self) {
        let _ = self.shutdown(ShutdownMode::CancelPending);
    }
}

fn worker<B: AgentBackend>(
    shared: Arc<Shared<B>>,
    agent: WorkerAgent<B>,
    index: usize,
) -> Option<B::Error> {
    let mut session: Option<B::Session> = None;
    loop {
        let job = {
            let mut queue = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                let eligible = queue
                    .jobs
                    .iter()
                    .position(|job| job.target.is_none_or(|target| target == index));
                if let Some(job) = eligible.and_then(|position| queue.jobs.remove(position)) {
                    shared.active.fetch_add(1, Ordering::AcqRel);
                    break Some(job);
                }
                if queue.closed {
                    break None;
                }
                queue = shared.ready.wait(queue).unwrap_or_else(|e| e.into_inner());
            }
        };
        let Some(mut job) = job else { break };
        if job.cancellation.is_cancelled() {
            let _ = job.result.send(Err(TaskError::Cancelled));
            shared.active.fetch_sub(1, Ordering::AcqRel);
            continue;
        }
        if session.is_none() {
            match agent.backend.open(&agent.session_config) {
                Ok(opened) => session = Some(opened),
                Err(error) => {
                    let _ = job.result.send(Err(TaskError::Open(error)));
                    shared.active.fetch_sub(1, Ordering::AcqRel);
                    continue;
                }
            }
        }
        let mut request = Some(job.request);
        let mut attempt = 1;
        let outcome = loop {
            if job.cancellation.is_cancelled() {
                break Err(TaskError::Cancelled);
            }
            let result = session.as_mut().expect("session just opened").run(
                request.take().expect("request for attempt"),
                &job.cancellation,
            );
            match result {
                Ok(_response) if job.cancellation.is_cancelled() => {
                    break Err(TaskError::Cancelled);
                }
                Ok(response) => break Ok(response),
                Err(source) => {
                    if !job.cancellation.is_cancelled()
                        && let Some(retry) = job.retry.as_mut()
                        && attempt < retry.max_attempts.get()
                        && session
                            .as_ref()
                            .expect("session just ran")
                            .can_retry_after(&source)
                        && let Some(next) = (retry.next_request)(&source, attempt)
                    {
                        request = Some(next);
                        attempt += 1;
                        continue;
                    }
                    let close = session.take().and_then(|mut session| session.close().err());
                    break Err(TaskError::Run { source, close });
                }
            }
        };
        let _ = job.result.send(outcome);
        shared.active.fetch_sub(1, Ordering::AcqRel);
    }
    session.and_then(|mut session| session.close().err())
}
