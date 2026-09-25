//! Four ACP processes, sixteen addition tasks, and an inspectable timeline.
//! Build the two helper binaries first, then run with --mock or --codex.

use std::collections::HashSet;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use agentpools::{AgentBackend, AgentPool, AgentSession, CancellationToken, ShutdownMode};
use agentpools_acp::{
    AcpBackend, AcpConfig, AcpError, AcpPrompt, AcpResponse, AcpSession, SharedAcpBackend,
    SharedAcpSession,
};
use serde::Serialize;
use serde_json::{Value, json};

const MODEL: &str = "gpt-6-luna";
const PROBLEMS: [(i64, i64); 16] = [
    (2, 3),
    (8, 13),
    (21, 34),
    (55, 89),
    (144, 233),
    (377, 610),
    (5, 17),
    (29, 43),
    (64, 128),
    (7, 71),
    (100, 250),
    (19, 23),
    (999, 1),
    (312, 488),
    (42, 58),
    (1234, 5678),
];

#[derive(Clone, Copy)]
enum Mode {
    Mock,
    Codex,
    CodexShared,
}

impl Mode {
    fn label(self) -> &'static str {
        match self {
            Self::Mock => "mock ACP + real MCP",
            Self::Codex => "Codex ACP (4 processes) + real MCP",
            Self::CodexShared => "Codex ACP (1 shared process) + real MCP",
        }
    }

    fn slug(self) -> &'static str {
        match self {
            Self::Mock => "mock",
            Self::Codex => "codex",
            Self::CodexShared => "codex-shared",
        }
    }

    fn is_codex(self) -> bool {
        matches!(self, Self::Codex | Self::CodexShared)
    }

    fn expected_processes(self) -> usize {
        if matches!(self, Self::CodexShared) { 1 } else { 4 }
    }
}

struct AdditionTask {
    id: usize,
    a: i64,
    b: i64,
    submitted_ms: u128,
}

#[derive(Clone, Serialize)]
struct TaskRecord {
    id: usize,
    a: i64,
    b: i64,
    expected: i64,
    agent: Option<usize>,
    process_id: Option<u32>,
    submitted_ms: u128,
    started_ms: Option<u128>,
    finished_ms: u128,
    answer: Option<String>,
    error: Option<String>,
    correct: bool,
    format_compliant: bool,
    mcp_called: bool,
}

#[derive(Serialize)]
struct Report {
    mode: &'static str,
    model: Option<&'static str>,
    agent_processes: usize,
    total_ms: u128,
    correct: usize,
    format_compliant: usize,
    mcp_verified: usize,
    tasks: Vec<TaskRecord>,
}

struct TracingBackend {
    agent: usize,
    mode: Mode,
    started: Instant,
    records: Arc<Mutex<Vec<TaskRecord>>>,
    shared_backend: Option<SharedAcpBackend>,
}

struct TracingSession {
    inner: TracedAcpSession,
    agent: usize,
    mode: Mode,
    started: Instant,
    records: Arc<Mutex<Vec<TaskRecord>>>,
}

enum TracedAcpSession {
    PerWorker(AcpSession),
    Shared(SharedAcpSession),
}

impl TracedAcpSession {
    fn process_id(&self) -> u32 {
        match self {
            Self::PerWorker(session) => session.process_id(),
            Self::Shared(session) => session.process_id(),
        }
    }

    fn selected_model(&self) -> Option<&str> {
        match self {
            Self::PerWorker(session) => session.selected_model(),
            Self::Shared(session) => session.selected_model(),
        }
    }
}

impl AgentSession<AcpPrompt, AcpResponse, AcpError> for TracedAcpSession {
    fn run(
        &mut self,
        request: AcpPrompt,
        cancellation: &CancellationToken,
    ) -> Result<AcpResponse, AcpError> {
        match self {
            Self::PerWorker(session) => session.run(request, cancellation),
            Self::Shared(session) => session.run(request, cancellation),
        }
    }

    fn close(&mut self) -> Result<(), AcpError> {
        match self {
            Self::PerWorker(session) => session.close(),
            Self::Shared(session) => session.close(),
        }
    }

    fn can_retry_after(&self, error: &AcpError) -> bool {
        match self {
            Self::PerWorker(session) => session.can_retry_after(error),
            Self::Shared(session) => session.can_retry_after(error),
        }
    }
}

impl AgentBackend for TracingBackend {
    type Config = AcpConfig;
    type Request = AdditionTask;
    type Response = bool;
    type Error = AcpError;
    type Session = TracingSession;

    fn open(&self, config: &Self::Config) -> Result<Self::Session, Self::Error> {
        let inner = match &self.shared_backend {
            Some(backend) => TracedAcpSession::Shared(backend.open(config)?),
            None => TracedAcpSession::PerWorker(AcpBackend.open(config)?),
        };
        if self.mode.is_codex() && inner.selected_model() != Some(MODEL) {
            return Err(AcpError::Protocol(
                "Codex selected a different model".into(),
            ));
        }
        Ok(TracingSession {
            inner,
            agent: self.agent,
            mode: self.mode,
            started: self.started,
            records: Arc::clone(&self.records),
        })
    }
}

impl AgentSession<AdditionTask, bool, AcpError> for TracingSession {
    fn run(
        &mut self,
        task: AdditionTask,
        cancellation: &CancellationToken,
    ) -> Result<bool, AcpError> {
        let started_ms = self.started.elapsed().as_millis();
        let prompt = match self.mode {
            Mode::Mock => format!("add {} and {}", task.a, task.b),
            Mode::Codex | Mode::CodexShared => format!(
                "Call the calculator MCP add tool with a={} and b={}. Then reply with only the integer result.",
                task.a, task.b
            ),
        };
        let result = self.inner.run(AcpPrompt::text(prompt), cancellation);
        let finished_ms = self.started.elapsed().as_millis();
        let answer = result.as_ref().ok().map(|response| response.text.clone());
        let actual = answer.as_deref().map(str::trim).map(|text| {
            if matches!(self.mode, Mode::Mock) {
                text.strip_prefix("mock:").unwrap_or(text)
            } else {
                text
            }
        });
        let expected_text = (task.a + task.b).to_string();
        let actual_value = actual.and_then(trailing_integer);
        let correct = actual_value == Some(task.a + task.b);
        let format_compliant = actual == Some(expected_text.as_str());
        self.records.lock().unwrap().push(TaskRecord {
            id: task.id,
            a: task.a,
            b: task.b,
            expected: task.a + task.b,
            agent: Some(self.agent),
            process_id: Some(self.inner.process_id()),
            submitted_ms: task.submitted_ms,
            started_ms: Some(started_ms),
            finished_ms,
            answer,
            error: result.as_ref().err().map(ToString::to_string),
            correct,
            format_compliant,
            mcp_called: false,
        });
        result.map(|_| correct)
    }

    fn close(&mut self) -> Result<(), AcpError> {
        self.inner.close()
    }

    fn can_retry_after(&self, error: &AcpError) -> bool {
        self.inner.can_retry_after(error)
    }
}

fn trailing_integer(text: &str) -> Option<i64> {
    let text = text.trim().trim_end_matches(['.', ',', ';', ':', '!', '?']);
    let bytes = text.as_bytes();
    let mut start = bytes.len();
    while start > 0 && bytes[start - 1].is_ascii_digit() {
        start -= 1;
    }
    if start == bytes.len() {
        return None;
    }
    if start > 0 && bytes[start - 1] == b'-' {
        start -= 1;
    }
    text.get(start..)?.parse().ok()
}

fn helper_binary(name: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .ok_or("cannot locate workspace root")?;
    let path = workspace
        .join("target")
        .join("debug")
        .join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    if !path.is_file() {
        return Err(format!(
            "missing helper binary {}; run cargo build --bins first",
            path.display()
        )
        .into());
    }
    Ok(path)
}

fn output_dir(mode: Mode) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .ok_or("cannot locate workspace root")?;
    let millis = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
    let path = root
        .join("target")
        .join("agentpools-batch-add")
        .join(format!("{}-{millis}", mode.slug()));
    fs::create_dir_all(&path)?;
    Ok(path)
}

fn config(
    mode: Mode,
    agent: usize,
    add_bin: &Path,
    out: &Path,
) -> Result<AcpConfig, Box<dyn std::error::Error>> {
    let cwd = std::env::current_dir()?;
    let mut config = match mode {
        Mode::Mock => {
            let mut config = AcpConfig::new(helper_binary("agentpools-acp-mock-agent")?, cwd);
            config
                .env
                .insert("AGENTPOOLS_MOCK_SCENARIO".into(), "mcp-add".into());
            config.env.insert(
                "AGENTPOOLS_MOCK_LOG".into(),
                out.join(format!("agent-{agent}-acp.jsonl"))
                    .into_os_string(),
            );
            config
                .env
                .insert("AGENTPOOLS_MOCK_DELAY_MS".into(), "120".into());
            config
        }
        Mode::Codex | Mode::CodexShared => {
            let entry = std::env::var_os("CODEX_ACP_ENTRY")
                .ok_or("set CODEX_ACP_ENTRY to the installed codex-acp dist/index.js")?;
            let entry = PathBuf::from(entry);
            if !entry.is_absolute() || !entry.is_file() {
                return Err("CODEX_ACP_ENTRY must point to an existing absolute file".into());
            }
            let mut config = AcpConfig::new("node", cwd);
            config.args.push(OsString::from(entry));
            config.model = Some(MODEL.into());
            config
        }
    };
    config.mcp_servers = vec![json!({
        "name": "calculator",
        "command": add_bin.to_string_lossy(),
        "args": [],
        "env": [{
            "name": "AGENTPOOLS_MCP_LOG",
            "value": out.join(format!("agent-{agent}-mcp.jsonl")).to_string_lossy()
        }]
    })];
    config.handshake_timeout = Duration::from_secs(60);
    config.prompt_timeout = Duration::from_secs(180);
    config.close_timeout = Duration::from_secs(5);
    Ok(config)
}

fn mark_mcp_calls(
    records: &mut [TaskRecord],
    out: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    for agent in 0..4 {
        let path = out.join(format!("agent-{agent}-mcp.jsonl"));
        if !path.exists() {
            continue;
        }
        let lines = fs::read_to_string(path)?;
        let calls: Vec<Value> = lines
            .lines()
            .map(serde_json::from_str)
            .collect::<Result<_, _>>()?;
        for record in records
            .iter_mut()
            .filter(|record| record.agent == Some(agent))
        {
            record.mcp_called = calls.iter().any(|call| {
                call["a"] == record.a
                    && call["b"] == record.b
                    && call["answer"]
                        .as_str()
                        .and_then(|value| value.parse::<i64>().ok())
                        == Some(record.expected)
            });
        }
    }
    Ok(())
}

fn write_report(report: &Report, out: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let json = serde_json::to_string_pretty(report)?;
    fs::write(out.join("report.json"), &json)?;
    let safe_json = json.replace('<', "\\u003c");
    let html = include_str!("batch_add_report.html").replace("__REPORT_JSON__", &safe_json);
    fs::write(out.join("report.html"), html)?;
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mode = match std::env::args().nth(1).as_deref() {
        Some("--mock") => Mode::Mock,
        Some("--codex") => Mode::Codex,
        Some("--codex-shared") => Mode::CodexShared,
        _ => return Err("choose --mock, --codex, or --codex-shared".into()),
    };
    let add_bin = helper_binary("agentpools-mcp-add")?;
    let out = output_dir(mode)?;
    let started = Instant::now();
    let records = Arc::new(Mutex::new(Vec::with_capacity(PROBLEMS.len())));
    let mut agents = Vec::new();
    let shared_backend = matches!(mode, Mode::CodexShared).then(SharedAcpBackend::new);
    for agent in 0..4 {
        agents.push((
            TracingBackend {
                agent,
                mode,
                started,
                records: Arc::clone(&records),
                shared_backend: shared_backend.clone(),
            },
            config(mode, agent, &add_bin, &out)?,
        ));
    }
    let mut pool = AgentPool::with_agents(agents, PROBLEMS.len())?;
    let mut handles = Vec::new();
    for (index, &(a, b)) in PROBLEMS.iter().enumerate() {
        let task = AdditionTask {
            id: index + 1,
            a,
            b,
            submitted_ms: started.elapsed().as_millis(),
        };
        let handle = if index < 4 {
            pool.submit_to(index, task)
        } else {
            pool.submit(task)
        }
        .map_err(|error| format!("cannot submit task {}: {error}", index + 1))?;
        handles.push(handle);
    }
    let mut failures = Vec::new();
    for (index, handle) in handles.into_iter().enumerate() {
        if let Err(error) = handle.wait() {
            failures.push((index + 1, error.to_string()));
        }
    }
    let shutdown = pool.shutdown(ShutdownMode::Drain);
    if shutdown.panicked_workers > 0 || !shutdown.close_errors.is_empty() {
        eprintln!(
            "worker shutdown: {} panics, {} close errors",
            shutdown.panicked_workers,
            shutdown.close_errors.len()
        );
    }
    let total_ms = started.elapsed().as_millis();
    let mut records = records.lock().unwrap().clone();
    for (id, error) in failures {
        if records.iter().any(|record| record.id == id) {
            continue;
        }
        let (a, b) = PROBLEMS[id - 1];
        records.push(TaskRecord {
            id,
            a,
            b,
            expected: a + b,
            agent: None,
            process_id: None,
            submitted_ms: 0,
            started_ms: None,
            finished_ms: total_ms,
            answer: None,
            error: Some(error),
            correct: false,
            format_compliant: false,
            mcp_called: false,
        });
    }
    records.sort_by_key(|record| record.id);
    mark_mcp_calls(&mut records, &out)?;
    let agent_processes = records
        .iter()
        .filter_map(|record| record.process_id)
        .collect::<HashSet<_>>()
        .len();
    let report = Report {
        mode: mode.label(),
        model: if mode.is_codex() {
            Some(MODEL)
        } else {
            None
        },
        agent_processes,
        total_ms,
        correct: records.iter().filter(|record| record.correct).count(),
        format_compliant: records
            .iter()
            .filter(|record| record.format_compliant)
            .count(),
        mcp_verified: records.iter().filter(|record| record.mcp_called).count(),
        tasks: records,
    };
    write_report(&report, &out)?;
    println!(
        "{}: {} ACP processes, {}/16 numerically correct, {}/16 integer-only, {}/16 verified MCP calls, {} ms",
        report.mode,
        report.agent_processes,
        report.correct,
        report.format_compliant,
        report.mcp_verified,
        report.total_ms
    );
    for task in &report.tasks {
        println!(
            "#{:02} {:>4}+{:<4} agent={:?} pid={:?} correct={} format={} mcp={} {}..{} ms",
            task.id,
            task.a,
            task.b,
            task.agent.map(|agent| agent + 1),
            task.process_id,
            task.correct,
            task.format_compliant,
            task.mcp_called,
            task.started_ms.unwrap_or(task.submitted_ms),
            task.finished_ms
        );
    }
    println!("JSON: {}", out.join("report.json").display());
    println!("HTML: {}", out.join("report.html").display());
    if report.agent_processes != mode.expected_processes()
        || report.correct != PROBLEMS.len()
        || report.mcp_verified != PROBLEMS.len()
    {
        return Err("batch run did not pass all answer and MCP-call checks".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::trailing_integer;

    #[test]
    fn parses_final_integer_after_global_instruction_prefix() {
        assert_eq!(
            trailing_integer(
                "Natural English: Call the calculator MCP add tool with a=2 and b=3. Then reply with only the integer result.5"
            ),
            Some(5)
        );
    }

    #[test]
    fn parses_integer_with_sentence_punctuation() {
        assert_eq!(trailing_integer("The answer is 72."), Some(72));
    }

    #[test]
    fn rejects_responses_without_a_trailing_integer() {
        assert_eq!(trailing_integer("I could not calculate that"), None);
    }
}
