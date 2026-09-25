use agentpools::{
    AgentBackend, AgentPool, AgentSession, CancellationToken, PoolConfig, ShutdownMode,
    SubmitError, TaskError, collect_ordered,
};
use std::num::NonZeroUsize;
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
fn opens_lazily_reuses_session_and_passes_exact_tool_config() {
    let state = Arc::new(State::default());
    let mut pool = pool(&state, 1, 4);
    assert_eq!(state.opens.load(Ordering::SeqCst), 0);
    assert_eq!(
        pool.submit(Request::simple(1)).unwrap().wait().unwrap(),
        (1, 1)
    );
    assert_eq!(
        pool.submit(Request::simple(2)).unwrap().wait().unwrap(),
        (2, 1)
    );
    let report = pool.shutdown(ShutdownMode::Drain);
    assert!(report.close_errors.is_empty());
    assert_eq!(report.panicked_workers, 0);
    assert_eq!(state.opens.load(Ordering::SeqCst), 1);
    assert_eq!(state.closes.load(Ordering::SeqCst), 1);
    assert_eq!(
        *state.configs.lock().unwrap(),
        vec![(
            1,
            SessionConfig {
                mcp_servers: vec!["caller-filesystem".into(), "caller-search".into()],
                tool_policy: "read-only".into(),
            }
        )]
    );
}

#[test]
fn two_agents_run_concurrently_and_results_keep_input_order() {
    let state = Arc::new(State::default());
    let mut pool = pool(&state, 2, 2);
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let (entered, receiver) = mpsc::channel();
    let first = pool
        .submit(Request::gated(10, entered.clone(), Arc::clone(&gate)))
        .unwrap();
    let second = pool
        .submit(Request::gated(20, entered, Arc::clone(&gate)))
        .unwrap();
    receiver.recv_timeout(Duration::from_secs(2)).unwrap();
    receiver.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(state.max_active.load(Ordering::SeqCst), 2);
    release(&gate);
    let outputs = collect_ordered([first, second]);
    assert_eq!(outputs[0].as_ref().unwrap().0, 10);
    assert_eq!(outputs[1].as_ref().unwrap().0, 20);
    pool.shutdown(ShutdownMode::Drain);
    assert_eq!(state.closes.load(Ordering::SeqCst), 2);
}

#[test]
fn each_worker_receives_its_own_agent_configuration() {
    let state = Arc::new(State::default());
    let make_agent = |name: &str| {
        (
            Backend(Arc::clone(&state)),
            SessionConfig {
                mcp_servers: vec![name.to_string()],
                tool_policy: "read-only".into(),
            },
        )
    };
    let mut pool = AgentPool::with_agents(
        vec![make_agent("agent-a-tools"), make_agent("agent-b-tools")],
        2,
    )
    .unwrap();
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let (entered, receiver) = mpsc::channel();
    let first = pool
        .submit_to(0, Request::gated(1, entered.clone(), Arc::clone(&gate)))
        .unwrap();
    let second = pool
        .submit_to(1, Request::gated(2, entered, Arc::clone(&gate)))
        .unwrap();
    assert!(matches!(
        pool.submit_to(2, Request::simple(3)),
        Err(SubmitError::NoSuchAgent(_))
    ));
    receiver.recv_timeout(Duration::from_secs(2)).unwrap();
    receiver.recv_timeout(Duration::from_secs(2)).unwrap();
    release(&gate);
    let outputs = collect_ordered([first, second]);
    let first_id = outputs[0].as_ref().unwrap().1;
    let second_id = outputs[1].as_ref().unwrap().1;
    pool.shutdown(ShutdownMode::Drain);
    let configs = state.configs.lock().unwrap();
    assert_eq!(
        configs
            .iter()
            .find(|(id, _)| *id == first_id)
            .unwrap()
            .1
            .mcp_servers,
        ["agent-a-tools"]
    );
    assert_eq!(
        configs
            .iter()
            .find(|(id, _)| *id == second_id)
            .unwrap()
            .1
            .mcp_servers,
        ["agent-b-tools"]
    );
}

#[test]
fn bounded_queue_returns_request_and_queued_task_can_be_cancelled() {
    let state = Arc::new(State::default());
    let mut pool = pool(&state, 1, 1);
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let (entered, receiver) = mpsc::channel();
    let running = pool
        .submit(Request::gated(1, entered, Arc::clone(&gate)))
        .unwrap();
    receiver.recv_timeout(Duration::from_secs(2)).unwrap();
    let queued = pool.submit(Request::simple(2)).unwrap();
    match pool.submit(Request::simple(3)) {
        Err(SubmitError::QueueFull(request)) => assert_eq!(request.value, 3),
        _ => panic!("third request must be returned to caller"),
    }
    queued.cancel();
    release(&gate);
    assert_eq!(running.wait().unwrap().0, 1);
    assert!(matches!(queued.wait(), Err(TaskError::Cancelled)));
    pool.shutdown(ShutdownMode::Drain);
    assert_eq!(state.opens.load(Ordering::SeqCst), 1);
}

#[test]
fn failed_task_closes_uncertain_session_and_next_task_opens_another() {
    let state = Arc::new(State::default());
    let mut pool = pool(&state, 1, 2);
    let mut failing = Request::simple(1);
    failing.fail = true;
    assert!(matches!(
        pool.submit(failing).unwrap().wait(),
        Err(TaskError::Run {
            source: "agent failed",
            close: None
        })
    ));
    assert_eq!(
        pool.submit(Request::simple(2)).unwrap().wait().unwrap(),
        (2, 2)
    );
    pool.shutdown(ShutdownMode::Drain);
    assert_eq!(state.opens.load(Ordering::SeqCst), 2);
    assert_eq!(state.closes.load(Ordering::SeqCst), 2);
}

#[test]
fn retry_keeps_same_session_and_worker_while_other_worker_progresses() {
    let state = Arc::new(State::default());
    let mut pool = pool(&state, 2, 4);
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let (failed, received) = mpsc::channel();
    let mut first_request = Request::simple(7);
    first_request.retryable_fail = true;
    let first = pool
        .submit_retrying_to(0, first_request, NonZeroUsize::new(2).unwrap(), {
            let gate = Arc::clone(&gate);
            move |error, attempt| {
                assert_eq!((*error, attempt), ("retryable", 1));
                failed.send(()).unwrap();
                let (lock, ready) = &*gate;
                let mut released = lock.lock().unwrap();
                while !*released {
                    released = ready.wait(released).unwrap();
                }
                Some(Request::simple(7))
            }
        })
        .unwrap();
    received.recv_timeout(Duration::from_secs(2)).unwrap();
    let queued_same_worker = pool.submit_to(0, Request::simple(8)).unwrap();
    let parallel = pool.submit_to(1, Request::simple(9)).unwrap();
    let parallel_result = parallel.wait().unwrap();
    assert_eq!(parallel_result.0, 9);
    assert_eq!(pool.status().active, 1);
    assert_eq!(pool.status().queued, 1);
    release(&gate);
    let retried = first.wait().unwrap();
    let queued = queued_same_worker.wait().unwrap();
    assert_eq!(retried, (7, queued.1));
    assert_ne!(retried.1, parallel_result.1);
    pool.shutdown(ShutdownMode::Drain);
    assert_eq!(state.opens.load(Ordering::SeqCst), 2);
    assert_eq!(state.closes.load(Ordering::SeqCst), 2);
}

#[test]
fn cancel_pending_shutdown_finishes_active_work_and_rejects_queue() {
    let state = Arc::new(State::default());
    let mut pool = pool(&state, 1, 2);
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let (entered, receiver) = mpsc::channel();
    let running = pool
        .submit(Request::gated(1, entered, Arc::clone(&gate)))
        .unwrap();
    receiver.recv_timeout(Duration::from_secs(2)).unwrap();
    let queued = pool.submit(Request::simple(2)).unwrap();
    let token = queued.cancellation_token();
    let releaser = std::thread::spawn({
        let gate = Arc::clone(&gate);
        move || release(&gate)
    });
    let report = pool.shutdown(ShutdownMode::CancelPending);
    releaser.join().unwrap();
    assert_eq!(report.panicked_workers, 0);
    assert!(token.is_cancelled());
    assert_eq!(running.wait().unwrap().0, 1);
    assert!(matches!(queued.wait(), Err(TaskError::Cancelled)));
    assert!(matches!(
        pool.submit(Request::simple(3)),
        Err(SubmitError::Closed(_))
    ));
}
