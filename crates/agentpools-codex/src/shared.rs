//! Multiplexed Codex app-server backend that shares one process across sessions.

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use agentpools::{ActivitySink, AgentBackend, AgentSession, CancellationToken, record_activity};
use serde_json::{Value, json};

use crate::{CodexConfig, CodexError, CodexPrompt, CodexResponse};

type Incoming = Result<Value, String>;

/// Opt-in Codex backend that multiplexes independent sessions over a single
/// `codex app-server` process. Each session owns its own `threadId`.
#[derive(Clone, Default)]
pub struct SharedCodexBackend {
    manager: Arc<SharedCodexManager>,
}

impl SharedCodexBackend {
    pub fn new() -> Self {
        Self::default()
    }
}

impl AgentBackend for SharedCodexBackend {
    type Config = CodexConfig;
    type Request = CodexPrompt;
    type Response = CodexResponse;
    type Error = CodexError;
    type Session = SharedCodexSession;

    fn open(&self, config: &Self::Config) -> Result<Self::Session, Self::Error> {
        self.manager.open(config)
    }
}

#[derive(Default)]
struct SharedCodexManager {
    state: Mutex<SharedManagerState>,
}

#[derive(Default)]
struct SharedManagerState {
    signature: Option<ProcessSignature>,
    process: Option<Arc<SharedCodexProcess>>,
}

#[derive(Clone, PartialEq, Eq)]
struct ProcessSignature {
    program: PathBuf,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    inherit_stderr: bool,
}

impl ProcessSignature {
    fn from_config(config: &CodexConfig) -> Self {
        Self {
            program: config.program.clone(),
            args: config.args.clone(),
            env: config.env.clone(),
            inherit_stderr: config.inherit_stderr,
        }
    }
}

impl SharedCodexManager {
    fn open(&self, config: &CodexConfig) -> Result<SharedCodexSession, CodexError> {
        if !config.cwd.is_absolute() {
            return Err(CodexError::InvalidConfig("cwd must be absolute"));
        }
        let signature = ProcessSignature::from_config(config);
        let process = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(existing) = &state.signature
                && existing != &signature
            {
                return Err(CodexError::InvalidConfig(
                    "shared Codex sessions must use the same process binary, args, and environment",
                ));
            }

            if let Some(process) = &state.process {
                Arc::clone(process)
            } else {
                let process = SharedCodexProcess::spawn(config)?;
                state.signature = Some(signature);
                state.process = Some(Arc::clone(&process));
                process
            }
        };

        let mut params = json!({
            "cwd": config.cwd.to_string_lossy(),
            "approvalPolicy": config.approval_policy,
            "ephemeral": config.ephemeral,
        });
        if let Some(model) = &config.model {
            params["model"] = json!(model);
        }
        if let Some(sandbox) = &config.sandbox {
            params["sandbox"] = json!(sandbox);
        }
        if let Some(mcp) = &config.mcp_servers {
            params["config"] = json!({ "mcp_servers": mcp });
        }

        let created = process.call(
            "thread/start",
            params,
            config.handshake_timeout,
            "thread/start",
        )?;
        let thread_id = created
            .pointer("/thread/id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| CodexError::Protocol("thread/start omitted thread id".into()))?
            .to_string();

        let events = process.register_session(&thread_id)?;

        Ok(SharedCodexSession {
            process,
            thread_id,
            events,
            ephemeral: config.ephemeral,
            prompt_timeout: config.prompt_timeout,
            close_timeout: config.close_timeout,
            closed: false,
            effort: config.effort.clone(),
            activity: None,
        })
    }
}

/// A session running on a shared `codex app-server` process.
///
/// Closing it deletes the thread (if `ephemeral: true`) and frees the `threadId`;
/// the shared process stays alive for other workers.
pub struct SharedCodexSession {
    process: Arc<SharedCodexProcess>,
    thread_id: String,
    events: Receiver<Incoming>,
    ephemeral: bool,
    prompt_timeout: Duration,
    close_timeout: Duration,
    closed: bool,
    effort: Option<String>,
    activity: Option<ActivitySink>,
}

impl SharedCodexSession {
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    pub fn process_id(&self) -> u32 {
        self.process.process_id
    }
}

impl AgentSession<CodexPrompt, CodexResponse, CodexError> for SharedCodexSession {
    fn run(
        &mut self,
        request: CodexPrompt,
        cancellation: &CancellationToken,
    ) -> Result<CodexResponse, CodexError> {
        if self.closed {
            return Err(CodexError::Protocol("session already closed".into()));
        }
        if cancellation.is_cancelled() {
            return Err(CodexError::Cancelled);
        }
        let text = request.extract_text()?;
        self.activity = request.activity.clone();

        let mut turn_params = json!({
            "threadId": self.thread_id,
            "input": [{"type": "text", "text": text}]
        });
        if let Some(effort) = &self.effort {
            turn_params["effort"] = json!(effort);
        }
        let start_res =
            self.process
                .call("turn/start", turn_params, self.prompt_timeout, "turn/start")?;
        let turn_id = start_res
            .pointer("/turn/id")
            .and_then(Value::as_str)
            .ok_or_else(|| CodexError::Protocol("turn/start returned no turn id".into()))?
            .to_owned();
        record_activity(&self.activity, "turn_started", Some(turn_id.clone()));

        let mut answer = String::new();
        let mut cancel_sent = false;
        let deadline = Instant::now() + self.prompt_timeout;

        loop {
            if cancellation.is_cancelled() && !cancel_sent {
                let _ = self.process.call(
                    "turn/interrupt",
                    json!({
                        "threadId": self.thread_id,
                        "turnId": turn_id
                    }),
                    self.close_timeout,
                    "turn/interrupt",
                );
                record_activity(&self.activity, "cancelled", Some(turn_id.clone()));
                cancel_sent = true;
            }

            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or(CodexError::Timeout("turn"))?;

            let event = match self
                .events
                .recv_timeout(remaining.min(Duration::from_millis(50)))
            {
                Ok(Ok(event)) => event,
                Ok(Err(err)) => return Err(CodexError::Protocol(err)),
                Err(RecvTimeoutError::Timeout) => {
                    if cancel_sent {
                        return Err(CodexError::Cancelled);
                    }
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(CodexError::Protocol("Codex router closed".into()));
                }
            };
            record_activity(&self.activity, "activity", Some(turn_id.clone()));

            match event.get("method").and_then(Value::as_str) {
                Some("item/completed") => {
                    let item = &event["params"]["item"];
                    if item["type"] == "agentMessage"
                        && item["phase"].as_str().is_none_or(|p| p == "final_answer")
                        && let Some(text) = item["text"].as_str()
                    {
                        answer = text.to_owned();
                    }
                }
                Some("turn/completed") => {
                    let event_turn_id = event.pointer("/params/turn/id").and_then(Value::as_str);
                    if event_turn_id == Some(&turn_id) {
                        let turn = &event["params"]["turn"];
                        if turn["status"] != "completed" {
                            return Err(CodexError::Remote(turn["error"].to_string()));
                        }
                        if answer.is_empty() {
                            return Err(CodexError::NoOutput);
                        }
                        record_activity(&self.activity, "turn_ended", Some(turn_id.clone()));
                        return Ok(CodexResponse {
                            text: answer,
                            stop_reason: "completed".into(),
                        });
                    }
                }
                _ => {}
            }
        }
    }

    fn close(&mut self) -> Result<(), CodexError> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        let mut result = Ok(());
        if self.ephemeral && !self.thread_id.is_empty() {
            // Best-effort ephemeral cleanup, same bound as the standalone
            // session: a busy server must not stall teardown/cancellation.
            let delete_res = self.process.call(
                "thread/delete",
                json!({
                    "threadId": self.thread_id
                }),
                self.close_timeout.min(Duration::from_secs(1)),
                "thread/delete",
            );
            // Ephemeral threads are never persisted server-side, so there is
            // nothing to delete; any other delete failure still counts.
            if let Err(e) = delete_res
                && !e.to_string().contains("not persisted")
            {
                result = Err(e);
            }
        }
        self.process.unregister_session(&self.thread_id);
        result
    }

    fn can_retry_after(&self, error: &CodexError) -> bool {
        !self.closed && matches!(error, CodexError::Remote(_) | CodexError::UnsupportedPrompt)
    }
}

impl Drop for SharedCodexSession {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

struct SharedCodexProcess {
    child: Mutex<Child>,
    stdin: Mutex<ChildStdin>,
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, Sender<Incoming>>>,
    sessions: Mutex<HashMap<String, Sender<Incoming>>>,
    reader: Mutex<Option<JoinHandle<()>>>,
    process_id: u32,
}

impl SharedCodexProcess {
    fn spawn(config: &CodexConfig) -> Result<Arc<Self>, CodexError> {
        let mut command = Command::new(&config.program);
        command
            .args(&config.args)
            .envs(&config.env)
            .current_dir(&config.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(if config.inherit_stderr {
                Stdio::inherit()
            } else {
                Stdio::null()
            });

        let mut child = command.spawn().map_err(CodexError::Spawn)?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| CodexError::Protocol("child has no stdin pipe".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| CodexError::Protocol("child has no stdout pipe".into()))?;

        let process_id = child.id();
        let process = Arc::new(Self {
            child: Mutex::new(child),
            stdin: Mutex::new(stdin),
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            reader: Mutex::new(None),
            process_id,
        });

        let weak = Arc::downgrade(&process);
        let reader = thread::Builder::new()
            .name("agentpools-codex-shared-reader".into())
            .spawn(move || read_messages(stdout, weak))
            .map_err(CodexError::Spawn)?;

        *process.reader.lock().unwrap_or_else(|e| e.into_inner()) = Some(reader);

        // Perform initialization
        let _ = process.call(
            "initialize",
            json!({
                "clientInfo": {
                    "name": "agentpools-codex-shared",
                    "title": "agentpools-codex-shared",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }),
            config.handshake_timeout,
            "initialize",
        )?;
        process.send(&json!({"method": "initialized", "params": {}}))?;

        Ok(process)
    }

    fn register_session(&self, thread_id: &str) -> Result<Receiver<Incoming>, CodexError> {
        let (sender, receiver) = mpsc::channel();
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        if sessions.contains_key(thread_id) {
            return Err(CodexError::Protocol(format!(
                "Codex reused active threadId {thread_id}"
            )));
        }
        sessions.insert(thread_id.to_string(), sender);
        Ok(receiver)
    }

    fn unregister_session(&self, thread_id: &str) {
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(thread_id);
    }

    fn send(&self, message: &Value) -> Result<(), CodexError> {
        let mut stdin = self.stdin.lock().unwrap_or_else(|e| e.into_inner());
        serde_json::to_writer(&mut *stdin, message)
            .map_err(|e| CodexError::Protocol(format!("Codex request encoding failed: {e}")))?;
        stdin.write_all(b"\n").map_err(CodexError::Io)?;
        stdin.flush().map_err(CodexError::Io)
    }

    fn call(
        &self,
        method: &'static str,
        params: Value,
        timeout: Duration,
        stage: &'static str,
    ) -> Result<Value, CodexError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, response) = mpsc::channel();
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, sender);

        if let Err(error) = self.send(&json!({
            "id": id,
            "method": method,
            "params": params
        })) {
            self.pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id);
            return Err(error);
        }

        match response.recv_timeout(timeout) {
            Ok(Ok(message)) => {
                if let Some(error) = message.get("error") {
                    let msg = error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown error");
                    return Err(CodexError::Remote(msg.to_string()));
                }
                Ok(message.get("result").cloned().unwrap_or(Value::Null))
            }
            Ok(Err(err)) => Err(CodexError::Protocol(err)),
            Err(RecvTimeoutError::Timeout) => {
                self.pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id);
                Err(CodexError::Timeout(stage))
            }
            Err(RecvTimeoutError::Disconnected) => {
                self.pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id);
                Err(CodexError::Protocol("Codex router disconnected".into()))
            }
        }
    }

    fn dispatch(&self, message: Value) {
        if message.get("method").and_then(Value::as_str).is_some() {
            let thread_id = message
                .pointer("/params/threadId")
                .or_else(|| message.pointer("/params/turn/threadId"))
                .or_else(|| message.pointer("/params/item/threadId"))
                .and_then(Value::as_str);

            let sender = thread_id.and_then(|tid| {
                self.sessions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(tid)
                    .cloned()
            });

            if let Some(sender) = sender {
                let _ = sender.send(Ok(message));
            }
            return;
        }

        if let Some(id) = message.get("id").and_then(Value::as_u64) {
            let sender = self
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id);
            if let Some(sender) = sender {
                let _ = sender.send(Ok(message));
            }
        }
    }

    fn fail_all(&self, error: String) {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        for (_, sender) in pending.drain() {
            let _ = sender.send(Err(error.clone()));
        }

        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        for (_, sender) in sessions.drain() {
            let _ = sender.send(Err(error.clone()));
        }
    }
}

impl Drop for SharedCodexProcess {
    fn drop(&mut self) {
        let mut child = self.child.lock().unwrap_or_else(|e| e.into_inner());
        let reader = self.reader.lock().unwrap_or_else(|e| e.into_inner()).take();
        let _ =
            agentpools_transport::terminate_child(&mut child, reader, Duration::from_millis(500));
    }
}

fn read_messages(stdout: impl std::io::Read, process: Weak<SharedCodexProcess>) {
    let mut stdout = BufReader::new(stdout);
    loop {
        let mut line = String::new();
        match stdout.read_line(&mut line) {
            Ok(0) => {
                if let Some(process) = process.upgrade() {
                    process.fail_all("Codex stdout closed".into());
                }
                break;
            }
            Ok(_) if line.trim().is_empty() => continue,
            Ok(_) => match serde_json::from_str::<Value>(&line) {
                Ok(message) => {
                    let Some(process) = process.upgrade() else {
                        break;
                    };
                    process.dispatch(message);
                }
                Err(error) => {
                    if let Some(process) = process.upgrade() {
                        process.fail_all(format!("invalid Codex JSON: {error}"));
                    }
                    break;
                }
            },
            Err(error) => {
                if let Some(process) = process.upgrade() {
                    process.fail_all(format!("Codex stdout read failed: {error}"));
                }
                break;
            }
        }
    }
}
