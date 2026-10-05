use crate::lease::{LeaseCommand, LeaseReady};
use crate::{
    AcquireError, AgentBackend, AgentSession, BuildError, CancellationToken, LeaseHandle,
    SessionLease, TaskError,
};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread::{self, JoinHandle};

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

struct LeaseRequest<B: AgentBackend> {
    target: Option<usize>,
    cancellation: CancellationToken,
    result: mpsc::Sender<Result<LeaseReady<B>, AcquireError<B::Error>>>,
}

struct Queue<B: AgentBackend> {
    jobs: VecDeque<LeaseRequest<B>>,
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
/// and are reused for later leases assigned to that worker.
pub struct AgentPool<B: AgentBackend> {
    shared: Arc<Shared<B>>,
    workers: Vec<JoinHandle<Option<B::Error>>>,
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

    /// Block until an idle worker has opened its session and reserve it until
    /// the returned lease is finished or dropped. Business validation between
    /// `ask` calls is part of the reservation.
    pub fn acquire(&self) -> Result<SessionLease<B>, AcquireError<B::Error>> {
        self.request_lease()?.wait()
    }

    /// Reserve a specific zero-based worker for a multi-round interaction.
    pub fn acquire_to(
        &self,
        agent_index: usize,
    ) -> Result<SessionLease<B>, AcquireError<B::Error>> {
        self.request_lease_to(agent_index)?.wait()
    }

    /// Queue an exclusive reservation without blocking the caller. This is
    /// useful for async runtimes and language bindings that wait elsewhere.
    pub fn request_lease(&self) -> Result<LeaseHandle<B>, AcquireError<B::Error>> {
        self.request_lease_routed(None)
    }

    /// Like `request_lease`, but reserve a chosen worker.
    pub fn request_lease_to(
        &self,
        agent_index: usize,
    ) -> Result<LeaseHandle<B>, AcquireError<B::Error>> {
        self.request_lease_routed(Some(agent_index))
    }

    fn request_lease_routed(
        &self,
        target: Option<usize>,
    ) -> Result<LeaseHandle<B>, AcquireError<B::Error>> {
        let mut queue = self.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
        if queue.closed {
            return Err(AcquireError::Closed);
        }
        if target.is_some_and(|index| index >= self.shared.agent_count) {
            return Err(AcquireError::NoSuchAgent);
        }
        if queue.jobs.len() >= self.shared.max_queued {
            return Err(AcquireError::QueueFull);
        }
        let cancellation = CancellationToken::default();
        let (sender, result) = mpsc::channel();
        queue.jobs.push_back(LeaseRequest {
            target,
            cancellation: cancellation.clone(),
            result: sender,
        });
        self.shared.ready.notify_all();
        Ok(LeaseHandle {
            cancellation: Some(cancellation),
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

    /// Stop accepting leases, then join workers and close their sessions.
    /// Active leases must be finished or dropped before this call can return.
    /// Running calls must finish cooperatively or reach their adapter timeout.
    pub fn shutdown(&mut self, mode: ShutdownMode) -> ShutdownReport<B::Error> {
        {
            let mut queue = self.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
            queue.closed = true;
            if mode == ShutdownMode::CancelPending {
                for request in queue.jobs.drain(..) {
                    request.cancellation.cancel();
                    let _ = request.result.send(Err(AcquireError::Cancelled));
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
        let work = {
            let mut queue = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                let eligible = queue
                    .jobs
                    .iter()
                    .position(|request| request.target.is_none_or(|target| target == index));
                if let Some(work) = eligible.and_then(|position| queue.jobs.remove(position)) {
                    shared.active.fetch_add(1, Ordering::AcqRel);
                    break Some(work);
                }
                if queue.closed {
                    break None;
                }
                queue = shared.ready.wait(queue).unwrap_or_else(|e| e.into_inner());
            }
        };
        let Some(work) = work else { break };
        let release_ack = run_lease(&agent, &mut session, index, work);
        shared.active.fetch_sub(1, Ordering::AcqRel);
        if let Some(ack) = release_ack {
            let _ = ack.send(());
        }
    }
    session.and_then(|mut session| session.close().err())
}

fn run_lease<B: AgentBackend>(
    agent: &WorkerAgent<B>,
    session: &mut Option<B::Session>,
    index: usize,
    request: LeaseRequest<B>,
) -> Option<mpsc::Sender<()>> {
    if request.cancellation.is_cancelled() {
        let _ = request.result.send(Err(AcquireError::Cancelled));
        return None;
    }
    if session.is_none() {
        match agent.backend.open(&agent.session_config) {
            Ok(opened) => *session = Some(opened),
            Err(error) => {
                let _ = request.result.send(Err(AcquireError::Open(error)));
                return None;
            }
        }
    }
    if request.cancellation.is_cancelled() {
        let _ = request.result.send(Err(AcquireError::Cancelled));
        return None;
    }
    let (commands, receiver) = mpsc::channel();
    if request
        .result
        .send(Ok(LeaseReady {
            agent_index: index,
            commands,
        }))
        .is_err()
    {
        return None;
    }
    loop {
        match receiver.recv() {
            Ok(LeaseCommand::Ask {
                request,
                cancellation,
                result,
            }) => {
                if cancellation.is_cancelled() {
                    let _ = result.send(Err(TaskError::Cancelled));
                    continue;
                }
                if session.is_none() {
                    match agent.backend.open(&agent.session_config) {
                        Ok(opened) => *session = Some(opened),
                        Err(error) => {
                            let _ = result.send(Err(TaskError::Open(error)));
                            continue;
                        }
                    }
                }
                let outcome = match session
                    .as_mut()
                    .expect("session just opened")
                    .run(request, &cancellation)
                {
                    Ok(_response) if cancellation.is_cancelled() => Err(TaskError::Cancelled),
                    Ok(response) => Ok(response),
                    // Cooperative cancellation keeps the session: tearing it
                    // down here would issue a follow-up protocol round-trip
                    // against a still-busy server, reintroducing the full
                    // turn latency and breaking lease reuse.
                    Err(_) if cancellation.is_cancelled() => Err(TaskError::Cancelled),
                    Err(source) => {
                        let close = if !cancellation.is_cancelled()
                            && session
                                .as_ref()
                                .expect("session just ran")
                                .can_retry_after(&source)
                        {
                            None
                        } else {
                            session.take().and_then(|mut session| session.close().err())
                        };
                        Err(TaskError::Run { source, close })
                    }
                };
                let _ = result.send(outcome);
            }
            Ok(LeaseCommand::Release { completed }) => return completed,
            Err(_) => return None,
        }
    }
}
