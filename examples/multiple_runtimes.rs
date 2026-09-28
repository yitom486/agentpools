//! Two independent adapter types can run at the same time without ACP.
//! These are in-memory examples, not Codex app-server or Pi implementations.

use agentpools::{
    AgentBackend, AgentPool, AgentSession, CancellationToken, PoolConfig, ShutdownMode,
};

struct TextBackend;
struct TextSession;

impl AgentBackend for TextBackend {
    type Config = ();
    type Request = String;
    type Response = String;
    type Error = String;
    type Session = TextSession;

    fn open(&self, _: &()) -> Result<TextSession, String> {
        Ok(TextSession)
    }
}

impl AgentSession<String, String, String> for TextSession {
    fn run(&mut self, request: String, _: &CancellationToken) -> Result<String, String> {
        Ok(request.to_uppercase())
    }

    fn close(&mut self) -> Result<(), String> {
        Ok(())
    }
}

struct CountBackend;
struct CountSession;

impl AgentBackend for CountBackend {
    type Config = ();
    type Request = Vec<u8>;
    type Response = usize;
    type Error = String;
    type Session = CountSession;

    fn open(&self, _: &()) -> Result<CountSession, String> {
        Ok(CountSession)
    }
}

impl AgentSession<Vec<u8>, usize, String> for CountSession {
    fn run(&mut self, request: Vec<u8>, _: &CancellationToken) -> Result<usize, String> {
        Ok(request.len())
    }

    fn close(&mut self) -> Result<(), String> {
        Ok(())
    }
}

fn main() {
    let config = PoolConfig {
        workers: 1,
        max_queued: 4,
    };
    let mut text_pool = AgentPool::new(TextBackend, (), config).expect("start text pool");
    let mut count_pool = AgentPool::new(CountBackend, (), config).expect("start count pool");

    // Each lease owns a worker in its own pool until it is released.
    let mut text = text_pool.acquire().expect("acquire text worker");
    let mut count = count_pool.acquire().expect("acquire count worker");
    assert_eq!(text_pool.status().active, 1);
    assert_eq!(count_pool.status().active, 1);
    assert_eq!(text.ask("hello".into()).expect("text response"), "HELLO");
    assert_eq!(count.ask(vec![1, 2, 3]).expect("count response"), 3);

    text.finish().expect("release text worker");
    count.finish().expect("release count worker");
    assert!(
        text_pool
            .shutdown(ShutdownMode::Drain)
            .close_errors
            .is_empty()
    );
    assert!(
        count_pool
            .shutdown(ShutdownMode::Drain)
            .close_errors
            .is_empty()
    );
}
