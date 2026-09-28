//! Concrete runtime selection for language bindings. The scheduler remains protocol independent.

mod mcp;
mod process;

pub use mcp::{McpServer, McpTransport, parse_mcp_servers};

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use agentpools::{AgentBackend, AgentPool, AgentSession, CancellationToken};
use agentpools_acp::{
    AcpAgentOptions, AcpBackend, AcpConfig, AcpError, AcpPoolOptions, AcpPrompt, AcpResponse,
    AcpSession,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use process::JsonProcess;

/// Binding-level prompt shared by ACP and native runtimes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuntimePrompt {
    pub content: Vec<Value>,
}

impl RuntimePrompt {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![json!({"type":"text","text":text.into()})],
        }
    }
}

/// Final text response shared by the language bindings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuntimeResponse {
    pub text: String,
    pub stop_reason: String,
}

impl From<AcpResponse> for RuntimeResponse {
    fn from(response: AcpResponse) -> Self {
        Self {
            text: response.text,
            stop_reason: response.stop_reason,
        }
    }
}

#[derive(Debug)]
pub enum RuntimeError {
    Acp(AcpError),
    Spawn(io::Error),
    Io(io::Error),
    Protocol(String),
    Remote(String),
    Timeout,
    Cancelled,
    UnsupportedPrompt,
    NoOutput,
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Acp(error) => write!(f, "{error}"),
            Self::Spawn(error) => write!(f, "cannot start agent runtime: {error}"),
            Self::Io(error) => write!(f, "agent runtime I/O failed: {error}"),
            Self::Protocol(error) => write!(f, "agent runtime protocol failed: {error}"),
            Self::Remote(error) => write!(f, "agent runtime returned an error: {error}"),
            Self::Timeout => write!(f, "agent runtime timed out"),
            Self::Cancelled => write!(f, "agent runtime request cancelled"),
            Self::UnsupportedPrompt => write!(
                f,
                "native runtime currently accepts only text content blocks"
            ),
            Self::NoOutput => write!(f, "agent runtime returned no text"),
        }
    }
}

impl std::error::Error for RuntimeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Acp(error) => Some(error),
            Self::Spawn(error) | Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum NativeKind {
    CodexAppServer,
    PiRpc,
}

pub struct NativeConfig {
    kind: NativeKind,
    program: String,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    cwd: PathBuf,
    model: Option<String>,
    mcp_servers: Vec<McpServer>,
    handshake_timeout: Duration,
    prompt_timeout: Duration,
    inherit_stderr: bool,
}

impl NativeConfig {
    /// Creates a configuration for a native Codex app-server runtime worker.
    pub fn codex(program: impl Into<String>, cwd: impl Into<PathBuf>) -> Self {
        Self::new(NativeKind::CodexAppServer, program, cwd)
    }

    /// Creates a configuration for a native Pi RPC runtime worker.
    pub fn pi(program: impl Into<String>, cwd: impl Into<PathBuf>) -> Self {
        Self::new(NativeKind::PiRpc, program, cwd)
    }

    fn new(kind: NativeKind, program: impl Into<String>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            kind,
            program: program.into(),
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: cwd.into(),
            model: None,
            mcp_servers: Vec::new(),
            handshake_timeout: Duration::from_millis(default_handshake_ms()),
            prompt_timeout: Duration::from_millis(default_prompt_ms()),
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

    pub fn with_mcp_server(mut self, server: McpServer) -> Self {
        self.mcp_servers.push(server);
        self
    }

    pub fn with_mcp_servers<I>(mut self, servers: I) -> Self
    where
        I: IntoIterator<Item = McpServer>,
    {
        self.mcp_servers.extend(servers);
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

    pub fn with_inherit_stderr(mut self, inherit: bool) -> Self {
        self.inherit_stderr = inherit;
        self
    }
}

pub enum RuntimeConfig {
    Acp(AcpConfig),
    Native(NativeConfig),
}

pub enum RuntimeSession {
    Acp(AcpSession),
    Codex(CodexSession),
    Pi(PiSession),
}

#[derive(Default, Clone, Copy)]
pub struct RuntimeBackend;

impl AgentBackend for RuntimeBackend {
    type Config = RuntimeConfig;
    type Request = RuntimePrompt;
    type Response = RuntimeResponse;
    type Error = RuntimeError;
    type Session = RuntimeSession;

    fn open(&self, config: &Self::Config) -> Result<Self::Session, Self::Error> {
        match config {
            RuntimeConfig::Acp(config) => AcpBackend
                .open(config)
                .map(RuntimeSession::Acp)
                .map_err(RuntimeError::Acp),
            RuntimeConfig::Native(config) => match config.kind {
                NativeKind::CodexAppServer => CodexSession::open(config).map(RuntimeSession::Codex),
                NativeKind::PiRpc => PiSession::open(config).map(RuntimeSession::Pi),
            },
        }
    }
}

impl AgentSession<RuntimePrompt, RuntimeResponse, RuntimeError> for RuntimeSession {
    fn run(
        &mut self,
        request: RuntimePrompt,
        cancellation: &CancellationToken,
    ) -> Result<RuntimeResponse, RuntimeError> {
        match self {
            Self::Acp(session) => session
                .run(
                    AcpPrompt {
                        content: request.content,
                    },
                    cancellation,
                )
                .map(RuntimeResponse::from)
                .map_err(RuntimeError::Acp),
            Self::Codex(session) => session.run(request, cancellation),
            Self::Pi(session) => session.run(request, cancellation),
        }
    }

    fn close(&mut self) -> Result<(), RuntimeError> {
        match self {
            Self::Acp(session) => session.close().map_err(RuntimeError::Acp),
            Self::Codex(session) => session.process.close(),
            Self::Pi(session) => session.process.close(),
        }
    }

    fn can_retry_after(&self, error: &RuntimeError) -> bool {
        match (self, error) {
            (Self::Acp(session), RuntimeError::Acp(error)) => session.can_retry_after(error),
            (_, RuntimeError::UnsupportedPrompt) => true,
            (Self::Codex(_), RuntimeError::Remote(_)) => true,
            (Self::Pi(_), RuntimeError::Remote(_)) => true,
            _ => false,
        }
    }
}

fn text_prompt(prompt: RuntimePrompt) -> Result<String, RuntimeError> {
    let mut text = Vec::with_capacity(prompt.content.len());
    for block in prompt.content {
        if block.get("type").and_then(Value::as_str) != Some("text") {
            return Err(RuntimeError::UnsupportedPrompt);
        }
        text.push(
            block
                .get("text")
                .and_then(Value::as_str)
                .ok_or(RuntimeError::UnsupportedPrompt)?
                .to_owned(),
        );
    }
    Ok(text.join("\n"))
}

pub struct CodexSession {
    process: JsonProcess,
    thread_id: String,
    next_id: u64,
    prompt_timeout: Duration,
}

impl CodexSession {
    fn open(config: &NativeConfig) -> Result<Self, RuntimeError> {
        let mut process = JsonProcess::spawn(config)?;
        let deadline = Instant::now() + config.handshake_timeout;
        process.send(&json!({"id":1,"method":"initialize","params":{"clientInfo":{"name":"agentpools","title":"agentpools","version":"0.1.0"}}}))?;
        process.response(1, deadline, None)?;
        process.send(&json!({"method":"initialized","params":{}}))?;
        let mut params = json!({"cwd": config.cwd, "approvalPolicy":"never"});
        if let Some(model) = &config.model {
            params["model"] = json!(model);
        }
        if !config.mcp_servers.is_empty() {
            let mut mcp_map = serde_json::Map::new();
            for server in &config.mcp_servers {
                let (name, value) = server.to_codex_entry();
                mcp_map.insert(name, value);
            }
            params["config"] = json!({
                "mcp_servers": Value::Object(mcp_map)
            });
        }
        process.send(&json!({"id":2,"method":"thread/start","params":params}))?;
        let result = process.response(2, deadline, None)?;
        let thread_id = result
            .pointer("/thread/id")
            .and_then(Value::as_str)
            .ok_or_else(|| RuntimeError::Protocol("thread/start returned no thread id".into()))?
            .to_owned();
        Ok(Self {
            process,
            thread_id,
            next_id: 3,
            prompt_timeout: config.prompt_timeout,
        })
    }

    fn run(
        &mut self,
        prompt: RuntimePrompt,
        cancellation: &CancellationToken,
    ) -> Result<RuntimeResponse, RuntimeError> {
        let text = text_prompt(prompt)?;
        let id = self.next_id;
        self.next_id += 1;
        self.process.send(&json!({"id":id,"method":"turn/start","params":{"threadId":self.thread_id,"input":[{"type":"text","text":text}]}}))?;
        let deadline = Instant::now() + self.prompt_timeout;
        let result = self.process.response(id, deadline, Some(cancellation))?;
        let turn_id = result
            .pointer("/turn/id")
            .and_then(Value::as_str)
            .ok_or_else(|| RuntimeError::Protocol("turn/start returned no turn id".into()))?
            .to_owned();
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
                cancel_sent = true;
            }
            let event = match self.process.next_event(deadline, cancellation) {
                Ok(event) => event,
                Err(RuntimeError::Cancelled) if !cancel_sent => {
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
                    return Err(RuntimeError::Cancelled);
                }
                Err(error) => return Err(error),
            };
            if event.get("id").is_some() && event.get("method").is_some() {
                return Err(RuntimeError::Protocol(
                    "app-server requested an unsupported host interaction".into(),
                ));
            }
            match event.get("method").and_then(Value::as_str) {
                Some("item/completed")
                    if event
                        .pointer("/params/turnId")
                        .and_then(Value::as_str)
                        .is_none_or(|value| value == turn_id) =>
                {
                    let item = &event["params"]["item"];
                    if item["type"] == "agentMessage"
                        && item["phase"]
                            .as_str()
                            .is_none_or(|phase| phase == "final_answer")
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
                        return Err(RuntimeError::Remote(turn["error"].to_string()));
                    }
                    if answer.is_empty() {
                        return Err(RuntimeError::NoOutput);
                    }
                    return Ok(RuntimeResponse {
                        text: answer,
                        stop_reason: "completed".into(),
                    });
                }
                _ => {}
            }
        }
    }
}

pub struct PiSession {
    process: JsonProcess,
    next_id: u64,
    prompt_timeout: Duration,
}

impl PiSession {
    fn open(config: &NativeConfig) -> Result<Self, RuntimeError> {
        let mut process = JsonProcess::spawn(config)?;
        process.send(&json!({"id":"agentpools-state","type":"get_state"}))?;
        let deadline = Instant::now() + config.handshake_timeout;
        loop {
            let reply = process.receive(deadline, None)?;
            if reply["id"] == "agentpools-state" && reply["type"] == "response" {
                if reply["success"] != true {
                    return Err(RuntimeError::Remote(reply["error"].to_string()));
                }
                break;
            }
        }
        Ok(Self {
            process,
            next_id: 1,
            prompt_timeout: config.prompt_timeout,
        })
    }

    fn run(
        &mut self,
        prompt: RuntimePrompt,
        cancellation: &CancellationToken,
    ) -> Result<RuntimeResponse, RuntimeError> {
        let text = text_prompt(prompt)?;
        let id = format!("prompt-{}", self.next_id);
        self.next_id += 1;
        self.process
            .send(&json!({"id":id,"type":"prompt","message":text}))?;
        let deadline = Instant::now() + self.prompt_timeout;
        let mut accepted = false;
        let mut settled = false;
        let mut answer = String::new();
        let mut stop_reason = "end".to_owned();
        let mut cancel_sent = false;
        loop {
            if cancellation.is_cancelled() && !cancel_sent {
                let cancel_id = format!("abort-{}", self.next_id);
                self.next_id += 1;
                let _ = self.process.send(&json!({
                    "id": cancel_id,
                    "type": "abort"
                }));
                cancel_sent = true;
            }
            let event = match self.process.next_event(deadline, cancellation) {
                Ok(event) => event,
                Err(RuntimeError::Cancelled) if !cancel_sent => {
                    let cancel_id = format!("abort-{}", self.next_id);
                    self.next_id += 1;
                    let _ = self.process.send(&json!({
                        "id": cancel_id,
                        "type": "abort"
                    }));
                    return Err(RuntimeError::Cancelled);
                }
                Err(error) => return Err(error),
            };
            match event.get("type").and_then(Value::as_str) {
                Some("response") if event["id"] == id => {
                    if event["success"] != true {
                        return Err(RuntimeError::Remote(event["error"].to_string()));
                    }
                    if event.pointer("/data/disposition").and_then(Value::as_str) == Some("handled")
                    {
                        return Err(RuntimeError::NoOutput);
                    }
                    accepted = true;
                }
                Some("message_end")
                    if event.pointer("/message/role").and_then(Value::as_str)
                        == Some("assistant") =>
                {
                    let message = &event["message"];
                    if let Some(blocks) = message["content"].as_array() {
                        answer = blocks
                            .iter()
                            .filter(|block| block["type"] == "text")
                            .filter_map(|block| block["text"].as_str())
                            .collect::<Vec<_>>()
                            .join("\n");
                    }
                    if let Some(reason) = message["stopReason"].as_str() {
                        stop_reason = reason.to_owned();
                    }
                }
                Some("agent_settled") => settled = true,
                Some("extension_ui_request") => {
                    return Err(RuntimeError::Protocol(
                        "Pi requested an unsupported extension UI interaction".into(),
                    ));
                }
                _ => {}
            }
            if accepted && settled {
                if stop_reason == "error" || stop_reason == "aborted" {
                    return Err(RuntimeError::Remote(format!(
                        "Pi finished with {stop_reason}"
                    )));
                }
                if answer.is_empty() {
                    return Err(RuntimeError::NoOutput);
                }
                return Ok(RuntimeResponse {
                    text: answer,
                    stop_reason,
                });
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct V2PoolOptions {
    api_version: u16,
    agents: Vec<Value>,
    #[serde(default = "default_max_queued")]
    max_queued: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct NativeAgentOptions {
    program: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    cwd: PathBuf,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    mcp_servers: Option<Value>,
    #[serde(default)]
    timeouts: NativeTimeouts,
    #[serde(default)]
    inherit_stderr: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct NativeTimeouts {
    #[serde(default = "default_handshake_ms")]
    handshake_ms: u64,
    #[serde(default = "default_prompt_ms")]
    prompt_ms: u64,
}

impl Default for NativeTimeouts {
    fn default() -> Self {
        Self {
            handshake_ms: default_handshake_ms(),
            prompt_ms: default_prompt_ms(),
        }
    }
}

fn default_max_queued() -> usize {
    128
}
fn default_handshake_ms() -> u64 {
    30_000
}
fn default_prompt_ms() -> u64 {
    300_000
}

impl NativeAgentOptions {
    fn into_config(self, kind: NativeKind) -> Result<NativeConfig, String> {
        if self.program.trim().is_empty() {
            return Err("program must not be empty".into());
        }
        if !self.cwd.is_absolute() {
            return Err("cwd must be an absolute path".into());
        }
        if self.timeouts.handshake_ms == 0 || self.timeouts.prompt_ms == 0 {
            return Err("timeouts must be greater than zero milliseconds".into());
        }
        if matches!(kind, NativeKind::PiRpc) && self.model.is_some() {
            return Err("set the Pi model through args, for example --model <id>".into());
        }
        let mcp_servers = if let Some(mcp) = &self.mcp_servers {
            parse_mcp_servers(mcp)?
        } else {
            Vec::new()
        };
        if matches!(kind, NativeKind::PiRpc) && !mcp_servers.is_empty() {
            return Err(
                "Pi RPC runtime does not currently support dynamic mcpServers configuration; configure tools via Pi extensions or args".into(),
            );
        }
        Ok(NativeConfig {
            kind,
            program: self.program,
            args: self.args,
            env: self.env,
            cwd: self.cwd,
            model: self.model,
            mcp_servers,
            handshake_timeout: Duration::from_millis(self.timeouts.handshake_ms),
            prompt_timeout: Duration::from_millis(self.timeouts.prompt_ms),
            inherit_stderr: self.inherit_stderr,
        })
    }
}

/// Parse either the existing ACP-only API v1 or API v2 with per-worker runtime tags.
pub fn build_pool(config_json: &str) -> Result<AgentPool<RuntimeBackend>, String> {
    let value: Value = serde_json::from_str(config_json).map_err(|error| error.to_string())?;
    let version = value
        .get("apiVersion")
        .and_then(Value::as_u64)
        .ok_or("apiVersion is required")?;
    let (agents, max_queued) = match version {
        1 => {
            let options: AcpPoolOptions =
                serde_json::from_value(value).map_err(|error| error.to_string())?;
            let max_queued = options.max_queued;
            let agents = options
                .agents
                .into_iter()
                .enumerate()
                .map(|(index, agent)| {
                    agent
                        .into_config(index)
                        .map(RuntimeConfig::Acp)
                        .map_err(|error| error.to_string())
                })
                .collect::<Result<Vec<_>, _>>()?;
            (agents, max_queued)
        }
        2 => {
            let options: V2PoolOptions =
                serde_json::from_value(value).map_err(|error| error.to_string())?;
            if options.api_version != 2 {
                return Err("expected apiVersion 2".into());
            }
            let mut agents = Vec::with_capacity(options.agents.len());
            for (index, mut value) in options.agents.into_iter().enumerate() {
                let runtime = value
                    .get("runtime")
                    .and_then(Value::as_str)
                    .ok_or_else(|| format!("agent {index}: runtime is required"))?
                    .to_owned();
                value
                    .as_object_mut()
                    .ok_or_else(|| format!("agent {index}: expected an object"))?
                    .remove("runtime");
                let config = match runtime.as_str() {
                    "acp" => {
                        if let Some(mcp) = value.get("mcpServers") {
                            let parsed = parse_mcp_servers(mcp)
                                .map_err(|error| format!("agent {index}: {error}"))?;
                            let acp_mcp: Vec<Value> =
                                parsed.iter().map(McpServer::to_acp_value).collect();
                            value["mcpServers"] = Value::Array(acp_mcp);
                        }
                        let agent: AcpAgentOptions = serde_json::from_value(value)
                            .map_err(|error| format!("agent {index}: {error}"))?;
                        RuntimeConfig::Acp(
                            agent
                                .into_config(index)
                                .map_err(|error| error.to_string())?,
                        )
                    }
                    "codexAppServer" | "piRpc" => {
                        let kind = if runtime == "codexAppServer" {
                            NativeKind::CodexAppServer
                        } else {
                            NativeKind::PiRpc
                        };
                        let agent: NativeAgentOptions = serde_json::from_value(value)
                            .map_err(|error| format!("agent {index}: {error}"))?;
                        RuntimeConfig::Native(
                            agent
                                .into_config(kind)
                                .map_err(|error| format!("agent {index}: {error}"))?,
                        )
                    }
                    _ => return Err(format!("agent {index}: unsupported runtime {runtime}")),
                };
                agents.push(config);
            }
            (agents, options.max_queued)
        }
        _ => return Err(format!("unsupported apiVersion {version}; expected 1 or 2")),
    };
    if agents.is_empty() {
        return Err("at least one agent must be configured".into());
    }
    if max_queued == 0 {
        return Err("maxQueued must be greater than zero".into());
    }
    AgentPool::with_agents(
        agents
            .into_iter()
            .map(|config| (RuntimeBackend, config))
            .collect(),
        max_queued,
    )
    .map_err(|error| error.to_string())
}
