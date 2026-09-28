use agentpools::{
    AcquireError, AgentBackend, AgentPool, AgentSession, CancellationToken, PoolConfig,
    ShutdownMode, TaskError,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Eq)]
struct SessionConfig {
    mcp_servers: Vec<String>,
    tool_policy: String,
}

#[derive(Default)]
struct State {
    opens: AtomicUsize,
    closes: AtomicUsize,
    active: AtomicUsize,
    max_active: AtomicUsize,
    configs: Mutex<Vec<(usize, SessionConfig)>>,
}

struct Backend(Arc<State>);

struct Session {
    state: Arc<State>,
    id: usize,
}

#[derive(Debug)]
struct Request {
    value: u32,
    fail: bool,
    retryable_fail: bool,
    entered: Option<mpsc::Sender<()>>,
    gate: Option<Arc<(Mutex<bool>, Condvar)>>,
}

impl Request {
    fn simple(value: u32) -> Self {
        Self {
            value,
            fail: false,
            retryable_fail: false,
            entered: None,
            gate: None,
        }
    }

    fn gated(value: u32, entered: mpsc::Sender<()>, gate: Arc<(Mutex<bool>, Condvar)>) -> Self {
        Self {
            value,
            fail: false,
            retryable_fail: false,
            entered: Some(entered),
            gate: Some(gate),
        }
    }
}

impl AgentBackend for Backend {
    type Config = SessionConfig;
    type Request = Request;
    type Response = (u32, usize);
    type Error = &'static str;
    type Session = Session;

    fn open(&self, config: &Self::Config) -> Result<Self::Session, Self::Error> {
        let mut configs = self.0.configs.lock().unwrap();
        let id = self.0.opens.fetch_add(1, Ordering::SeqCst) + 1;
        configs.push((id, config.clone()));
        Ok(Session {
            state: Arc::clone(&self.0),
            id,
        })
    }
}

impl AgentSession<Request, (u32, usize), &'static str> for Session {
    fn run(
        &mut self,
        request: Request,
        _cancellation: &CancellationToken,
    ) -> Result<(u32, usize), &'static str> {
        let active = self.state.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.state.max_active.fetch_max(active, Ordering::SeqCst);
        if let Some(entered) = request.entered {
            entered.send(()).unwrap();
        }
        if let Some(gate) = request.gate {
            let (lock, ready) = &*gate;
            let mut released = lock.lock().unwrap();
            while !*released {
                released = ready.wait(released).unwrap();
            }
        }
        self.state.active.fetch_sub(1, Ordering::SeqCst);
        if request.fail {
            Err("agent failed")
        } else if request.retryable_fail {
            Err("retryable")
        } else {
            Ok((request.value, self.id))
        }
    }

    fn close(&mut self) -> Result<(), &'static str> {
        self.state.closes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn can_retry_after(&self, error: &&'static str) -> bool {
        *error == "retryable"
    }
}

fn pool(state: &Arc<State>, workers: usize, max_queued: usize) -> AgentPool<Backend> {
    AgentPool::new(
        Backend(Arc::clone(state)),
        SessionConfig {
            mcp_servers: vec!["caller-filesystem".into(), "caller-search".into()],
            tool_policy: "read-only".into(),
        },
        PoolConfig {
            workers,
            max_queued,
        },
    )
    .unwrap()
}

fn release(gate: &Arc<(Mutex<bool>, Condvar)>) {
    let (lock, ready) = &**gate;
    *lock.lock().unwrap() = true;
    ready.notify_all();
}

#[test]
fn lease_reuses_session_and_forwards_exact_configuration() {
    let state = Arc::new(State::default());
    let mut pool = pool(&state, 1, 4);
    assert_eq!(state.opens.load(Ordering::SeqCst), 0);
    let mut lease = pool.acquire().unwrap();
    assert_eq!(lease.ask(Request::simple(1)).unwrap(), (1, 1));
    assert_eq!(lease.ask(Request::simple(2)).unwrap(), (2, 1));
    lease.finish().unwrap();
    let report = pool.shutdown(ShutdownMode::Drain);
    assert!(report.close_errors.is_empty());
    assert_eq!(state.opens.load(Ordering::SeqCst), 1);
    assert_eq!(state.closes.load(Ordering::SeqCst), 1);
    assert_eq!(
        state.configs.lock().unwrap()[0].1.mcp_servers,
        ["caller-filesystem", "caller-search"]
    );
}

#[test]
fn distinct_workers_run_concurrently() {
    let state = Arc::new(State::default());
    let mut pool = pool(&state, 2, 2);
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let (entered, receiver) = mpsc::channel();
    let mut first = pool.acquire_to(0).unwrap();
    let mut second = pool.acquire_to(1).unwrap();
    let first_gate = Arc::clone(&gate);
    let second_gate = Arc::clone(&gate);
    let first_call = std::thread::spawn(move || {
        let result = first.ask(Request::gated(10, entered, first_gate));
        first.finish().unwrap();
        result.unwrap()
    });
    let (second_entered, second_receiver) = mpsc::channel();
    let second_call = std::thread::spawn(move || {
        let result = second.ask(Request::gated(20, second_entered, second_gate));
        second.finish().unwrap();
        result.unwrap()
    });
    receiver.recv_timeout(Duration::from_secs(2)).unwrap();
    second_receiver
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    assert_eq!(state.max_active.load(Ordering::SeqCst), 2);
    release(&gate);
    assert_eq!(first_call.join().unwrap().0, 10);
    assert_eq!(second_call.join().unwrap().0, 20);
    pool.shutdown(ShutdownMode::Drain);
}

#[test]
fn lease_reserves_worker_during_external_validation() {
    let state = Arc::new(State::default());
    let mut pool = pool(&state, 2, 4);
    let mut lease = pool.acquire_to(0).unwrap();
    let first = lease.ask(Request::simple(7)).unwrap();
    let queued = pool.request_lease_to(0).unwrap();
    let mut parallel = pool.acquire_to(1).unwrap();
    assert_eq!(parallel.ask(Request::simple(9)).unwrap().0, 9);
    parallel.finish().unwrap();
    assert_eq!(pool.status().active, 1);
    assert_eq!(pool.status().queued, 1);
    assert_eq!(lease.ask(Request::simple(10)).unwrap().1, first.1);
    lease.finish().unwrap();
    let mut next = queued.wait().unwrap();
    assert_eq!(next.ask(Request::simple(8)).unwrap(), (8, first.1));
    next.finish().unwrap();
    pool.shutdown(ShutdownMode::Drain);
}

#[test]
fn queued_lease_can_be_cancelled_and_queue_is_bounded() {
    let state = Arc::new(State::default());
    let mut pool = pool(&state, 1, 1);
    let lease = pool.acquire().unwrap();
    let queued = pool.request_lease().unwrap();
    assert!(matches!(pool.request_lease(), Err(AcquireError::QueueFull)));
    queued.cancel();
    assert!(matches!(queued.wait(), Err(AcquireError::Cancelled)));
    lease.finish().unwrap();
    pool.shutdown(ShutdownMode::Drain);
}

#[test]
fn recoverable_error_keeps_session_and_uncertain_error_reopens_it() {
    let state = Arc::new(State::default());
    let mut pool = pool(&state, 1, 2);
    let mut lease = pool.acquire().unwrap();
    let mut retryable = Request::simple(1);
    retryable.retryable_fail = true;
    assert!(matches!(
        lease.ask(retryable),
        Err(TaskError::Run {
            source: "retryable",
            ..
        })
    ));
    assert_eq!(lease.ask(Request::simple(2)).unwrap(), (2, 1));
    let mut uncertain = Request::simple(3);
    uncertain.fail = true;
    assert!(matches!(
        lease.ask(uncertain),
        Err(TaskError::Run {
            source: "agent failed",
            ..
        })
    ));
    assert_eq!(lease.ask(Request::simple(4)).unwrap(), (4, 2));
    lease.finish().unwrap();
    pool.shutdown(ShutdownMode::Drain);
    assert_eq!(state.opens.load(Ordering::SeqCst), 2);
}
