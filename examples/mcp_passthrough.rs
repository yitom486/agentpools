use agentpools::{
    AgentBackend, AgentPool, AgentSession, CancellationToken, PoolConfig, ShutdownMode,
};

// This belongs to the caller's ACP adapter, not to agentpools.
struct AcpLaunch {
    mcp_servers: Vec<String>,
    tool_policy: String,
}

struct DemoAcpAdapter;

struct DemoSession {
    mcp_servers: Vec<String>,
    tool_policy: String,
}

impl AgentBackend for DemoAcpAdapter {
    type Config = AcpLaunch;
    type Request = String;
    type Response = String;
    type Error = String;
    type Session = DemoSession;

    fn open(&self, config: &AcpLaunch) -> Result<DemoSession, String> {
        // A real adapter would forward these fields in its session/new call.
        Ok(DemoSession {
            mcp_servers: config.mcp_servers.clone(),
            tool_policy: config.tool_policy.clone(),
        })
    }
}

impl AgentSession<String, String, String> for DemoSession {
    fn run(&mut self, request: String, _: &CancellationToken) -> Result<String, String> {
        Ok(format!(
            "{request}: tools={:?}, policy={}",
            self.mcp_servers, self.tool_policy
        ))
    }

    fn close(&mut self) -> Result<(), String> {
        Ok(())
    }
}

fn main() {
    let mut pool = AgentPool::new(
        DemoAcpAdapter,
        AcpLaunch {
            mcp_servers: vec!["caller-filesystem".into(), "caller-search".into()],
            tool_policy: "read-only".into(),
        },
        PoolConfig::default(),
    )
    .expect("start pool");
    let handle = pool.submit("summarize chapter 1".into()).expect("submit");
    println!("{}", handle.wait().expect("agent response"));
    let report = pool.shutdown(ShutdownMode::Drain);
    assert!(report.close_errors.is_empty());
}
