//! Native stdio Codex app-server adapter for [`agentpools`].
//!
//! Provides both standalone process-per-worker ([`CodexBackend`]) and
//! multiplexed single-process multi-session ([`SharedCodexBackend`]) execution.
//!
//! Sessions default to ephemeral mode (`ephemeral: true`), which permanently
//! deletes the session and its local rollout history from disk via `thread/delete`
//! upon closure.

mod shared;

pub use shared::{SharedCodexBackend, SharedCodexSession};

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use agentpools::{ActivitySink, AgentBackend, AgentSession, CancellationToken, record_activity};
use agentpools_transport::{JsonProcess, ProcessConfig, TransportError};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Configuration for Codex app-server workers.
#[derive(Debug, Clone)]
pub struct CodexConfig {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: PathBuf,
    pub model: Option<String>,
    /// Reasoning effort passed to every `turn/start` (mirrors the host Codex
    /// adapter: per-turn `effort`). Absent means the server default.
    pub effort: Option<String>,
    /// Opaque JSON object mapping MCP server names to server configurations,
    /// passed to `thread/start.params.config.mcp_servers`.
    pub mcp_servers: Option<Value>,
    /// Approval policy for model actions (default `"never"`).
    pub approval_policy: String,
    /// Sandbox mode for model-executed shell commands (`read-only` |
    /// `workspace-write` | `danger-full-access`, kebab-case). Absent means
    /// the server default; textbook workers need `workspace-write` so the
    /// file reader can write its audit log and rendered pages under cwd.
    pub sandbox: Option<String>,
    /// If `true` (the default), permanently deletes the session from disk on
    /// close via `thread/delete`, preventing session accumulation.
    pub ephemeral: bool,
    pub handshake_timeout: Duration,
    pub prompt_timeout: Duration,
    pub close_timeout: Duration,
    pub inherit_stderr: bool,
}

impl CodexConfig {
    pub fn new(program: impl Into<PathBuf>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: cwd.into(),
            model: None,
            effort: None,
            mcp_servers: None,
            approval_policy: "never".to_string(),
            sandbox: None,
            ephemeral: true,
            handshake_timeout: Duration::from_secs(30),
            prompt_timeout: Duration::from_secs(300),
            close_timeout: Duration::from_secs(5),
            inherit_stderr: false,
        }
    }

    pub fn with_arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn with_args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    pub fn with_effort(mut self, effort: impl Into<String>) -> Self {
        self.effort = Some(effort.into());
        self
    }

    pub fn with_mcp_servers(mut self, servers: Value) -> Self {
        self.mcp_servers = Some(servers);
        self
    }

    pub fn with_approval_policy(mut self, policy: impl Into<String>) -> Self {
        self.approval_policy = policy.into();
        self
    }

    pub fn with_sandbox(mut self, sandbox: impl Into<String>) -> Self {
        self.sandbox = Some(sandbox.into());
        self
    }

    pub fn with_ephemeral(mut self, ephemeral: bool) -> Self {
        self.ephemeral = ephemeral;
        self
    }

    pub fn with_handshake_timeout(mut self, timeout: Duration) -> Self {
        self.handshake_timeout = timeout;
        self
    }

    pub fn with_prompt_timeout(mut self, timeout: Duration) -> Self {
        self.prompt_timeout = timeout;
        self
    }

    pub fn with_close_timeout(mut self, timeout: Duration) -> Self {
        self.close_timeout = timeout;
        self
    }

    pub fn with_inherit_stderr(mut self, inherit: bool) -> Self {
        self.inherit_stderr = inherit;
        self
    }
}

/// Prompt input for Codex sessions.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodexPrompt {
    pub content: Vec<Value>,
    /// Cooperative activity sink for this ask; hosts drain it to prove liveness.
    /// Never serialized: set by the runtime binding, not by prompt JSON.
    #[serde(skip)]
    pub activity: Option<ActivitySink>,
}

impl CodexPrompt {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![json!({"type": "text", "text": text.into()})],
            activity: None,
        }
    }

    pub fn extract_text(&self) -> Result<String, CodexError> {
        let mut text = Vec::with_capacity(self.content.len());
        for block in &self.content {
            if block.get("type").and_then(Value::as_str) != Some("text") {
                return Err(CodexError::UnsupportedPrompt);
            }
            text.push(
                block
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or(CodexError::UnsupportedPrompt)?
                    .to_owned(),
            );
        }
        Ok(text.join("\n"))
    }
}

/// Response returned by a Codex session turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodexResponse {
    pub text: String,
    pub stop_reason: String,
}

#[derive(Debug)]
pub enum CodexError {
    InvalidConfig(&'static str),
    Spawn(io::Error),
    Io(io::Error),
    Protocol(String),
    Remote(String),
    Timeout(&'static str),
    Cancelled,
    UnsupportedPrompt,
    NoOutput,
}

impl fmt::Display for CodexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig(msg) => write!(f, "invalid Codex config: {msg}"),
            Self::Spawn(err) => write!(f, "cannot start Codex app-server: {err}"),
            Self::Io(err) => write!(f, "Codex app-server I/O failed: {err}"),
            Self::Protocol(msg) => write!(f, "Codex app-server protocol failed: {msg}"),
            Self::Remote(err) => write!(f, "Codex app-server returned error: {err}"),
            Self::Timeout(stage) => write!(f, "Codex app-server {stage} timed out"),
            Self::Cancelled => write!(f, "Codex request cancelled"),
            Self::UnsupportedPrompt => {
                write!(f, "Codex currently accepts only text content blocks")
            }
            Self::NoOutput => write!(f, "Codex returned no text"),
        }
    }
}

impl std::error::Error for CodexError {}

impl From<TransportError> for CodexError {
    fn from(err: TransportError) -> Self {
        match err {
            TransportError::Spawn(e) => Self::Spawn(e),
            TransportError::Io(e) => Self::Io(e),
            TransportError::Protocol(msg) => Self::Protocol(msg),
            TransportError::Remote(msg) => Self::Remote(msg),
            TransportError::Timeout => Self::Timeout("request"),
            TransportError::Cancelled => Self::Cancelled,
        }
    }
}

/// Standalone backend where each session owns an isolated `codex app-server` process.
#[derive(Default, Clone, Copy)]
pub struct CodexBackend;

impl AgentBackend for CodexBackend {
    type Config = CodexConfig;
    type Request = CodexPrompt;
    type Response = CodexResponse;
    type Error = CodexError;
    type Session = CodexSession;

    fn open(&self, config: &Self::Config) -> Result<Self::Session, Self::Error> {
        CodexSession::open(config)
    }
}

/// A standalone Codex session running in a dedicated child process.
pub struct CodexSession {
    process: JsonProcess,
    thread_id: String,
    next_id: u64,
    ephemeral: bool,
    prompt_timeout: Duration,
    close_timeout: Duration,
    closed: bool,
    effort: Option<String>,
    activity: Option<ActivitySink>,
    /// `turn/start` request ids whose response wait was cancelled before the
    /// turn id arrived. Their late responses surface as stale events on the
    /// next turn; the turn id extracted then is interrupted immediately.
    abandoned_starts: Vec<u64>,
}

impl CodexSession {
    pub fn open(config: &CodexConfig) -> Result<Self, CodexError> {
        let process_config = ProcessConfig {
            program: config.program.clone(),
            args: config.args.iter().map(OsString::from).collect(),
            env: config
                .env
                .iter()
                .map(|(k, v)| (OsString::from(k), OsString::from(v)))
                .collect(),
            cwd: config.cwd.clone(),
            inherit_stderr: config.inherit_stderr,
        };
        let mut process = JsonProcess::spawn(&process_config)?;
        let deadline = Instant::now() + config.handshake_timeout;
        process.send(&json!({
            "id": 1,
            "method": "initialize",
            "params": {
                "clientInfo": {
                    "name": "agentpools-codex",
                    "title": "agentpools-codex",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }
        }))?;
        process.response(1, deadline, None)?;
        process.send(&json!({"method": "initialized", "params": {}}))?;

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

        process.send(&json!({
            "id": 2,
            "method": "thread/start",
            "params": params
        }))?;
        let result = process.response(2, deadline, None)?;
        let thread_id = result
            .pointer("/thread/id")
            .and_then(Value::as_str)
            .ok_or_else(|| CodexError::Protocol("thread/start returned no thread id".into()))?
            .to_owned();

        Ok(Self {
            process,
            thread_id,
            next_id: 3,
            ephemeral: config.ephemeral,
            prompt_timeout: config.prompt_timeout,
            close_timeout: config.close_timeout,
            closed: false,
            effort: config.effort.clone(),
            activity: None,
            abandoned_starts: Vec::new(),
        })
    }

    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    /// If `event` answers an abandoned `turn/start` (a late response to a
    /// request whose wait was cancelled), consume the request id and return
    /// the orphaned turn id when the server actually started one (an error
    /// response means the turn never started, so there is nothing to
    /// interrupt). Returns `None` for all live traffic.
    fn harvest_abandoned_start(&mut self, event: &Value) -> Option<Option<String>> {
        let response_id = event.get("id").and_then(Value::as_u64)?;
        if event.get("method").is_some() {
            return None;
        }
        let position = self
            .abandoned_starts
            .iter()
            .position(|id| *id == response_id)?;
        self.abandoned_starts.remove(position);
        Some(
            event
                .pointer("/result/turn/id")
                .and_then(Value::as_str)
                .map(str::to_owned),
        )
    }
}

impl AgentSession<CodexPrompt, CodexResponse, CodexError> for CodexSession {
    fn run(
        &mut self,
        request: CodexPrompt,
        cancellation: &CancellationToken,
    ) -> Result<CodexResponse, CodexError> {
        if self.closed {
            return Err(CodexError::Protocol("session already closed".into()));
        }
        let text = request.extract_text()?;
        self.activity = request.activity.clone();
        let id = self.next_id;
        self.next_id += 1;
        let mut turn_params = json!({
            "threadId": self.thread_id,
            "input": [{"type": "text", "text": text}]
        });
        if let Some(effort) = &self.effort {
            turn_params["effort"] = json!(effort);
        }
        self.process.send(&json!({
            "id": id,
            "method": "turn/start",
            "params": turn_params
        }))?;
        let deadline = Instant::now() + self.prompt_timeout;
        let result = match self.process.response(id, deadline, Some(cancellation)) {
            Ok(result) => result,
            Err(TransportError::Cancelled) => {
                // No turn id yet, so no `turn/interrupt` can be addressed.
                // Remember the request: its late response is harvested on a
                // later turn and interrupted once the turn id is known.
                record_activity(&self.activity, "cancelled", None);
                self.abandoned_starts.push(id);
                return Err(CodexError::Cancelled);
            }
            Err(error) => return Err(CodexError::from(error)),
        };
        let turn_id = result
            .pointer("/turn/id")
            .and_then(Value::as_str)
            .ok_or_else(|| CodexError::Protocol("turn/start returned no turn id".into()))?
            .to_owned();
        record_activity(&self.activity, "turn_started", Some(turn_id.clone()));

        let mut answer = String::new();
        let mut cancel_sent = false;

        loop {
            if cancellation.is_cancelled() && !cancel_sent {
                let cancel_id = self.next_id;
                self.next_id += 1;
                let _ = self.process.send(&json!({
                    "id": cancel_id,
                    "method": "turn/interrupt",
                    "params": {
                        "threadId": self.thread_id,
                        "turnId": turn_id
                    }
                }));
                record_activity(&self.activity, "cancelled", Some(turn_id.clone()));
                cancel_sent = true;
            }

            let event = match self.process.next_event(deadline, cancellation) {
                Ok(event) => event,
                Err(TransportError::Cancelled) if !cancel_sent => {
                    let cancel_id = self.next_id;
                    self.next_id += 1;
                    let _ = self.process.send(&json!({
                        "id": cancel_id,
                        "method": "turn/interrupt",
                        "params": {
                            "threadId": self.thread_id,
                            "turnId": turn_id
                        }
                    }));
                    record_activity(&self.activity, "cancelled", Some(turn_id.clone()));
                    return Err(CodexError::Cancelled);
                }
                Err(error) => return Err(CodexError::from(error)),
            };
            if let Some(orphaned) = self.harvest_abandoned_start(&event) {
                if let Some(turn_id) = orphaned {
                    let cancel_id = self.next_id;
                    self.next_id += 1;
                    let _ = self.process.send(&json!({
                        "id": cancel_id,
                        "method": "turn/interrupt",
                        "params": {
                            "threadId": self.thread_id,
                            "turnId": turn_id
                        }
                    }));
                    record_activity(&self.activity, "cancelled", Some(turn_id));
                }
                continue;
            }
            record_activity(&self.activity, "activity", Some(turn_id.clone()));

            if event.get("id").is_some() && event.get("method").is_some() {
                let method = event.get("method").and_then(Value::as_str).unwrap_or("?");
                let params = event
                    .get("params")
                    .map(|value| value.to_string())
                    .unwrap_or_default();
                const MAX_PARAMS: usize = 2000;
                return Err(CodexError::Protocol(format!(
                    "app-server requested an unsupported host interaction: method={method} params={}",
                    params.chars().take(MAX_PARAMS).collect::<String>(),
                )));
            }

            match event.get("method").and_then(Value::as_str) {
                Some("item/completed")
                    if event
                        .pointer("/params/turnId")
                        .and_then(Value::as_str)
                        .is_none_or(|val| val == turn_id) =>
                {
                    let item = &event["params"]["item"];
                    if item["type"] == "agentMessage"
                        && item["phase"].as_str().is_none_or(|p| p == "final_answer")
                        && let Some(text) = item["text"].as_str()
                    {
                        answer = text.to_owned();
                    }
                }
                Some("turn/completed")
                    if event.pointer("/params/turn/id").and_then(Value::as_str)
                        == Some(turn_id.as_str()) =>
                {
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
                _ => {}
            }
        }
    }

    fn close(&mut self) -> Result<(), CodexError> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        if self.ephemeral && !self.thread_id.is_empty() {
            let delete_id = self.next_id;
            self.next_id += 1;
            let _ = self.process.send(&json!({
                "id": delete_id,
                "method": "thread/delete",
                "params": {
                    "threadId": self.thread_id
                }
            }));
            // Best-effort ephemeral cleanup: never let a busy or hung server
            // stall session teardown (and therefore cancellation) longer than
            // a short grace period. The process itself is closed below.
            let deadline = Instant::now() + self.close_timeout.min(Duration::from_secs(1));
            let _ = self.process.response(delete_id, deadline, None);
        }
        self.process.close().map_err(CodexError::from)
    }

    fn can_retry_after(&self, error: &CodexError) -> bool {
        !self.closed && matches!(error, CodexError::Remote(_) | CodexError::UnsupportedPrompt)
    }
}

impl Drop for CodexSession {
    fn drop(&mut self) {
        let _ = self.close();
    }
}
