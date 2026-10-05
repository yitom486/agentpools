//! Opt-in ACP backend that multiplexes independent sessions over one process.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use agentpools::{AgentBackend, AgentSession, CancellationToken};
use serde_json::{Value, json};

use crate::{
    AcpConfig, AcpError, AcpPrompt, AcpResponse, HostRequestHandler, append_text_update,
    response_id, response_result, selected_model,
};

type Incoming = Result<Value, String>;

/// ACP backend that shares one stdio process across all sessions opened from
/// clones of this backend. Each session still has its own ACP `sessionId`.
///
/// Use this as an explicit alternative to [`crate::AcpBackend`], which keeps
/// the existing one-process-per-worker behavior.
#[derive(Clone, Default)]
pub struct SharedAcpBackend {
    manager: Arc<SharedManager>,
}

impl SharedAcpBackend {
    pub fn new() -> Self {
        Self::default()
    }
}

impl AgentBackend for SharedAcpBackend {
    type Config = AcpConfig;
    type Request = AcpPrompt;
    type Response = AcpResponse;
    type Error = AcpError;
    type Session = SharedAcpSession;

    fn open(&self, config: &Self::Config) -> Result<Self::Session, Self::Error> {
        self.manager.open(config)
    }
}

#[derive(Default)]
struct SharedManager {
    state: Mutex<ManagerState>,
}

#[derive(Default)]
struct ManagerState {
    signature: Option<ProcessSignature>,
    process: Option<Arc<SharedProcess>>,
    supports_close: bool,
}

#[derive(Clone, PartialEq)]
struct ProcessSignature {
    program: PathBuf,
    args: Vec<OsString>,
    env: HashMap<OsString, OsString>,
    auth_method: Option<String>,
    client_capabilities: Value,
    inherit_stderr: bool,
}

impl ProcessSignature {
    fn from_config(config: &AcpConfig) -> Self {
        Self {
            program: config.program.clone(),
            args: config.args.clone(),
            env: config.env.clone(),
            auth_method: config.auth_method.clone(),
            client_capabilities: config.client_capabilities.clone(),
            inherit_stderr: config.inherit_stderr,
        }
    }
}

pub(crate) fn shared_process_profile_mismatch(configs: &[AcpConfig]) -> Option<usize> {
    let first = configs.first()?;
    let signature = ProcessSignature::from_config(first);
    configs
        .iter()
        .position(|config| ProcessSignature::from_config(config) != signature)
}

impl SharedManager {
    fn open(&self, config: &AcpConfig) -> Result<SharedAcpSession, AcpError> {
        validate_config(config)?;
        let signature = ProcessSignature::from_config(config);
        let (process, supports_close) = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if let Some(existing) = &state.signature
                && existing != &signature
            {
                return Err(AcpError::InvalidConfig(
                    "shared ACP sessions must use the same process, auth, and client capability settings",
                ));
            }

            if let Some(process) = &state.process {
                (Arc::clone(process), state.supports_close)
            } else {
                let process = SharedProcess::spawn(config)?;
                let initialized = process.call(
                    "initialize",
                    json!({
                        "protocolVersion": 1,
                        "clientInfo": {
                            "name": "agentpools-acp",
                            "version": env!("CARGO_PKG_VERSION")
                        },
                        "clientCapabilities": config.client_capabilities
                    }),
                    config.handshake_timeout,
                    "initialize",
                    None,
                    None,
                    None,
                    &mut |_| {},
                )?;
                let version = initialized.get("protocolVersion").and_then(Value::as_u64);
                if version != Some(1) {
                    return Err(AcpError::Protocol(format!(
                        "unsupported ACP version: {version:?}"
                    )));
                }
                let supports_close = initialized
                    .pointer("/agentCapabilities/sessionCapabilities/close")
                    .or_else(|| initialized.pointer("/agentCapabilities/session/close"))
                    .is_some_and(|value| !value.is_null() && value != false);
                if let Some(method) = &config.auth_method {
                    process.call(
                        "authenticate",
                        json!({ "methodId": method }),
                        config.handshake_timeout,
                        "authenticate",
                        None,
                        None,
                        None,
                        &mut |_| {},
                    )?;
                }
                state.signature = Some(signature);
                state.supports_close = supports_close;
                state.process = Some(Arc::clone(&process));
                (process, supports_close)
            }
        };

        let mut session_new_params = json!({
            "cwd": config.cwd.to_string_lossy(),
            "mcpServers": config.mcp_servers
        });
        if config.ephemeral {
            session_new_params["ephemeral"] = json!(true);
        }

        let created = process.call(
            "session/new",
            session_new_params,
            config.handshake_timeout,
            "session/new",
            None,
            None,
            None,
            &mut |_| {},
        )?;
        let session_id = created
            .get("sessionId")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| AcpError::Protocol("session/new omitted sessionId".into()))?
            .to_string();
        let events = process.register_session(&session_id)?;
        let mut session = SharedAcpSession {
            process,
            session_id,
            events,
            supports_close,
            ephemeral: config.ephemeral,
            handler: crate::permission_handler(config),
            prompt_timeout: config.prompt_timeout,
            close_timeout: config.close_timeout,
            selected_model: selected_model(&created).map(str::to_string),
            closed: false,
        };

        if let Some(requested_model) = &config.model {
            let result = session.call(
                "session/set_config_option",
                json!({
                    "sessionId": session.session_id,
                    "configId": "model",
                    "value": requested_model
                }),
                config.handshake_timeout,
                "set model",
                None,
                &mut |_| {},
            );
            let response = match result {
                Ok(response) => response,
                Err(error) => {
                    let _ = session.close();
                    return Err(error);
                }
            };
            let actual = selected_model(&response).ok_or_else(|| {
                AcpError::Protocol("model config response omitted selected model".into())
            })?;
            if actual != requested_model {
                let error = AcpError::Protocol(format!(
                    "requested model {requested_model}, Agent selected {actual}"
                ));
                let _ = session.close();
                return Err(error);
            }
            session.selected_model = Some(actual.to_string());
        }
        Ok(session)
    }
}

/// One session on a shared ACP process. Closing it only closes its ACP session;
/// the shared process stays alive for the pool's other workers.
pub struct SharedAcpSession {
    process: Arc<SharedProcess>,
    session_id: String,
    events: Receiver<Incoming>,
    supports_close: bool,
    ephemeral: bool,
    handler: Option<Arc<dyn HostRequestHandler>>,
    prompt_timeout: Duration,
    close_timeout: Duration,
    selected_model: Option<String>,
    closed: bool,
}

impl SharedAcpSession {
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Operating system process ID shared by this backend's sessions.
    pub fn process_id(&self) -> u32 {
        self.process.process_id
    }

    pub fn selected_model(&self) -> Option<&str> {
        self.selected_model.as_deref()
    }

    fn call(
        &mut self,
        method: &'static str,
        params: Value,
        timeout: Duration,
        stage: &'static str,
        cancellation: Option<&CancellationToken>,
        on_message: &mut dyn FnMut(&Value),
    ) -> Result<Value, AcpError> {
        self.process.call(
            method,
            params,
            timeout,
            stage,
            Some(&self.events),
            self.handler.as_deref(),
            cancellation.map(|token| (token, self.session_id.as_str(), self.close_timeout)),
            on_message,
        )
    }
}

impl AgentSession<AcpPrompt, AcpResponse, AcpError> for SharedAcpSession {
    fn run(
        &mut self,
        request: AcpPrompt,
        cancellation: &CancellationToken,
    ) -> Result<AcpResponse, AcpError> {
        if self.closed {
            return Err(AcpError::Protocol("session already closed".into()));
        }
        if cancellation.is_cancelled() {
            return Err(AcpError::Cancelled);
        }
        let mut text = String::new();
        let result = self.call(
            "session/prompt",
            json!({ "sessionId": self.session_id, "prompt": request.content }),
            self.prompt_timeout,
            "prompt",
            Some(cancellation),
            &mut |message| append_text_update(&mut text, message),
        )?;
        if text.is_empty() {
            return Err(AcpError::NoOutput);
        }
        Ok(AcpResponse {
            text,
            stop_reason: result
                .get("stopReason")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string(),
        })
    }

    fn close(&mut self) -> Result<(), AcpError> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        let delete_result = if self.ephemeral {
            let _ = self.call(
                "session/delete",
                json!({ "sessionId": self.session_id }),
                self.close_timeout,
                "session/delete",
                None,
                &mut |_| {},
            );
            Ok(())
        } else {
            Ok(())
        };
        let close_result = if self.supports_close {
            self.call(
                "session/close",
                json!({ "sessionId": self.session_id }),
                self.close_timeout,
                "session/close",
                None,
                &mut |_| {},
            )
            .map(|_| ())
        } else {
            Ok(())
        };
        self.process.unregister_session(&self.session_id);
        delete_result.and(close_result)
    }

    fn can_retry_after(&self, error: &AcpError) -> bool {
        !self.closed && matches!(error, AcpError::Remote { .. })
    }
}

impl Drop for SharedAcpSession {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

struct SharedProcess {
    child: Mutex<Child>,
    stdin: Mutex<ChildStdin>,
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, Sender<Incoming>>>,
    sessions: Mutex<HashMap<String, Sender<Incoming>>>,
    reader: Mutex<Option<JoinHandle<()>>>,
    process_id: u32,
}

impl SharedProcess {
    fn spawn(config: &AcpConfig) -> Result<Arc<Self>, AcpError> {
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
        let mut child = command.spawn().map_err(AcpError::Spawn)?;
        let Some(stdin) = child.stdin.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(AcpError::Protocol("agent process has no stdin pipe".into()));
        };
        let Some(stdout) = child.stdout.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(AcpError::Protocol(
                "agent process has no stdout pipe".into(),
            ));
        };
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
            .name("agentpools-acp-shared-reader".into())
            .spawn(move || read_messages(stdout, weak))
            .map_err(AcpError::Spawn)?;
        *process
            .reader
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(reader);
        Ok(process)
    }

    fn register_session(&self, session_id: &str) -> Result<Receiver<Incoming>, AcpError> {
        let (sender, receiver) = mpsc::channel();
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if sessions.contains_key(session_id) {
            return Err(AcpError::Protocol(format!(
                "Agent reused active sessionId {session_id}"
            )));
        }
        sessions.insert(session_id.to_string(), sender);
        Ok(receiver)
    }

    fn unregister_session(&self, session_id: &str) {
        self.sessions
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(session_id);
    }

    fn send(&self, message: &Value) -> Result<(), AcpError> {
        let mut stdin = self.stdin.lock().unwrap_or_else(|error| error.into_inner());
        serde_json::to_writer(&mut *stdin, message)
            .map_err(|error| AcpError::Protocol(format!("ACP request encoding failed: {error}")))?;
        stdin.write_all(b"\n").map_err(AcpError::Io)?;
        stdin.flush().map_err(AcpError::Io)
    }

    #[allow(clippy::too_many_arguments)]
    fn call(
        &self,
        method: &'static str,
        params: Value,
        timeout: Duration,
        stage: &'static str,
        events: Option<&Receiver<Incoming>>,
        handler: Option<&dyn HostRequestHandler>,
        cancel_context: Option<(&CancellationToken, &str, Duration)>,
        on_message: &mut dyn FnMut(&Value),
    ) -> Result<Value, AcpError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, response) = mpsc::channel();
        self.pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(id, sender);
        if let Err(error) = self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        })) {
            self.pending
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .remove(&id);
            return Err(error);
        }

        let prompt_deadline = Instant::now() + timeout;
        let mut cancel_deadline = None;
        let outcome = (|| loop {
            if let Some((cancellation, session_id, cancel_timeout)) = cancel_context
                && cancellation.is_cancelled()
                && cancel_deadline.is_none()
            {
                self.send(&json!({
                    "jsonrpc": "2.0",
                    "method": "session/cancel",
                    "params": { "sessionId": session_id }
                }))?;
                cancel_deadline = Some(Instant::now() + cancel_timeout);
            }

            let deadline = cancel_deadline.unwrap_or(prompt_deadline);
            let remaining = match deadline.checked_duration_since(Instant::now()) {
                Some(remaining) => remaining,
                None if cancel_deadline.is_some() => return Err(AcpError::Cancelled),
                None => return Err(AcpError::Timeout(stage)),
            };
            match response.recv_timeout(remaining.min(Duration::from_millis(20))) {
                Ok(Ok(message)) => {
                    drain_session_events(self, events, handler, on_message)?;
                    if cancel_deadline.is_some()
                        || cancel_context
                            .is_some_and(|(cancellation, _, _)| cancellation.is_cancelled())
                    {
                        return Err(AcpError::Cancelled);
                    }
                    return response_result(&message);
                }
                Ok(Err(error)) => return Err(AcpError::Protocol(error)),
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(AcpError::Protocol("ACP response router closed".into()));
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
            drain_session_events(self, events, handler, on_message)?;
        })();
        self.pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&id);
        outcome
    }
}

fn drain_session_events(
    process: &SharedProcess,
    events: Option<&Receiver<Incoming>>,
    handler: Option<&dyn HostRequestHandler>,
    on_message: &mut dyn FnMut(&Value),
) -> Result<(), AcpError> {
    let Some(events) = events else {
        return Ok(());
    };
    loop {
        match events.try_recv() {
            Ok(Ok(message)) => {
                if let Some(id) = message.get("id") {
                    let response = host_request_reply(id, &message, handler);
                    process.send(&response)?;
                } else {
                    on_message(&message);
                }
            }
            Ok(Err(error)) => return Err(AcpError::Protocol(error)),
            Err(TryRecvError::Empty) => return Ok(()),
            Err(TryRecvError::Disconnected) => {
                return Err(AcpError::Protocol("ACP session router closed".into()));
            }
        }
    }
}

impl Drop for SharedProcess {
    fn drop(&mut self) {
        let mut child = self.child.lock().unwrap_or_else(|error| error.into_inner());
        let reader = self
            .reader
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        let _ =
            agentpools_transport::terminate_child(&mut child, reader, Duration::from_millis(500));
    }
}

fn read_messages(stdout: impl std::io::Read, process: Weak<SharedProcess>) {
    let mut stdout = BufReader::new(stdout);
    loop {
        let mut line = String::new();
        match stdout.read_line(&mut line) {
            Ok(0) => {
                if let Some(process) = process.upgrade() {
                    process.fail_all("ACP stdout closed".into());
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
                        process.fail_all(format!("invalid ACP JSON: {error}"));
                    }
                    break;
                }
            },
            Err(error) => {
                if let Some(process) = process.upgrade() {
                    process.fail_all(format!("ACP stdout read failed: {error}"));
                }
                break;
            }
        }
    }
}

impl SharedProcess {
    fn dispatch(&self, message: Value) {
        if message.get("method").and_then(Value::as_str).is_some() {
            let session_id = message.pointer("/params/sessionId").and_then(Value::as_str);
            let sender = session_id.and_then(|session_id| {
                self.sessions
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .get(session_id)
                    .cloned()
            });
            if let Some(sender) = sender {
                let _ = sender.send(Ok(message));
            } else if let Some(id) = message.get("id") {
                let _ = self.send(&json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32601, "message": "unknown ACP session for agent request" }
                }));
            }
        } else if let Some(id) = response_id(&message) {
            let sender = self
                .pending
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .remove(&id);
            if let Some(sender) = sender {
                let _ = sender.send(Ok(message));
            }
        }
    }

    fn fail_all(&self, error: String) {
        let pending = std::mem::take(
            &mut *self
                .pending
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()),
        );
        for sender in pending.into_values() {
            let _ = sender.send(Err(error.clone()));
        }
        let sessions = self
            .sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        for sender in sessions.values() {
            let _ = sender.send(Err(error.clone()));
        }
    }
}

fn host_request_reply(
    id: &Value,
    message: &Value,
    handler: Option<&dyn HostRequestHandler>,
) -> Value {
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let params = message.get("params").unwrap_or(&Value::Null);
    let result = if let Some(handler) = handler {
        handler.handle(method, params)
    } else if method == "session/request_permission" {
        Ok(json!({ "outcome": { "outcome": "cancelled" } }))
    } else {
        Err(AcpError::Protocol(format!(
            "client does not implement {method}"
        )))
    };
    match result {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err(error) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32601, "message": error.to_string() }
        }),
    }
}

fn validate_config(config: &AcpConfig) -> Result<(), AcpError> {
    if !config.cwd.is_absolute() {
        return Err(AcpError::InvalidConfig("cwd must be absolute"));
    }
    if config.program.as_os_str().is_empty() {
        return Err(AcpError::InvalidConfig("program is empty"));
    }
    if config.handshake_timeout.is_zero()
        || config.prompt_timeout.is_zero()
        || config.close_timeout.is_zero()
    {
        return Err(AcpError::InvalidConfig("timeouts must be positive"));
    }
    if !config.client_capabilities.is_object() {
        return Err(AcpError::InvalidConfig(
            "client_capabilities must be an object",
        ));
    }
    Ok(())
}
