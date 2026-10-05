//! Concrete runtime selection for language bindings. The scheduler remains protocol independent.

mod mcp;
mod process;

pub use mcp::{McpServer, McpTransport, parse_mcp_servers};

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use agentpools::{ActivitySink, AgentBackend, AgentPool, AgentSession, CancellationToken};
use agentpools_acp::{
    AcpAgentOptions, AcpBackend, AcpConfig, AcpError, AcpPoolOptions, AcpPrompt, AcpResponse,
    AcpSession,
};
pub use agentpools_codex::{
    CodexBackend, CodexConfig, CodexError, CodexPrompt, CodexResponse, CodexSession,
    SharedCodexBackend, SharedCodexSession,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use process::JsonProcess;

/// Binding-level prompt shared by ACP and native runtimes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuntimePrompt {
    pub content: Vec<Value>,
    /// Cooperative activity sink for this ask. Never part of prompt JSON:
    /// skipped by serde, attached by the language binding per call.
    #[serde(skip)]
    pub activity: Option<ActivitySink>,
}

impl RuntimePrompt {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![json!({"type":"text","text":text.into()})],
            activity: None,
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

impl From<CodexResponse> for RuntimeResponse {
    fn from(response: CodexResponse) -> Self {
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

impl From<CodexError> for RuntimeError {
    fn from(error: CodexError) -> Self {
        match error {
            CodexError::Spawn(e) => Self::Spawn(e),
            CodexError::Io(e) => Self::Io(e),
            CodexError::Protocol(msg) => Self::Protocol(msg),
            CodexError::Remote(msg) => Self::Remote(msg),
            CodexError::Timeout(_) => Self::Timeout,
            CodexError::Cancelled => Self::Cancelled,
            CodexError::UnsupportedPrompt => Self::UnsupportedPrompt,
            CodexError::NoOutput => Self::NoOutput,
            CodexError::InvalidConfig(msg) => Self::Protocol(msg.to_string()),
        }
    }
}

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
    effort: Option<String>,
    mcp_servers: Vec<McpServer>,
    /// Codex thread approval policy (default `"never"` = deny anything that
    /// needs approval). Unattended MCP tool calls require a policy that lets
    /// them run, e.g. `"untrusted"`.
    approval_policy: String,
    /// Sandbox for model-executed shell commands (official SandboxMode).
    /// Absent = server default.
    sandbox: Option<String>,
    ephemeral: bool,
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
            effort: None,
            mcp_servers: Vec::new(),
            approval_policy: "never".to_owned(),
            sandbox: None,
            ephemeral: true,
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

    pub fn with_inherit_stderr(mut self, inherit: bool) -> Self {
        self.inherit_stderr = inherit;
        self
    }

    pub fn to_codex_config(&self) -> CodexConfig {
        let mut config = CodexConfig::new(&self.program, &self.cwd)
            .with_args(&self.args)
            .with_handshake_timeout(self.handshake_timeout)
            .with_prompt_timeout(self.prompt_timeout)
            .with_inherit_stderr(self.inherit_stderr)
            .with_ephemeral(self.ephemeral);

        for (k, v) in &self.env {
            config = config.with_env(k, v);
        }
        if let Some(model) = &self.model {
            config = config.with_model(model);
        }
        if let Some(effort) = &self.effort {
            config = config.with_effort(effort);
        }
        config = config.with_approval_policy(self.approval_policy.clone());
        if let Some(sandbox) = &self.sandbox {
            config = config.with_sandbox(sandbox);
        }
        if !self.mcp_servers.is_empty() {
            let mut mcp_map = serde_json::Map::new();
            for server in &self.mcp_servers {
                let (name, value) = server.to_codex_entry();
                mcp_map.insert(name, value);
            }
            config = config.with_mcp_servers(Value::Object(mcp_map));
        }
        config
    }
}

pub enum RuntimeConfig {
    Acp(AcpConfig),
    Native(NativeConfig),
    SharedCodex(SharedCodexBackend, CodexConfig),
}

pub enum RuntimeSession {
    Acp(AcpSession),
    Codex(CodexSession),
    SharedCodex(SharedCodexSession),
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
                NativeKind::CodexAppServer => {
                    let codex_config = config.to_codex_config();
                    CodexSession::open(&codex_config)
                        .map(RuntimeSession::Codex)
                        .map_err(RuntimeError::from)
                }
                NativeKind::PiRpc => PiSession::open(config).map(RuntimeSession::Pi),
            },
            RuntimeConfig::SharedCodex(backend, config) => backend
                .open(config)
                .map(RuntimeSession::SharedCodex)
                .map_err(RuntimeError::from),
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
            Self::Codex(session) => session
                .run(
                    CodexPrompt {
                        content: request.content,
                        activity: request.activity,
                    },
                    cancellation,
                )
                .map(RuntimeResponse::from)
                .map_err(RuntimeError::from),
            Self::SharedCodex(session) => session
                .run(
                    CodexPrompt {
                        content: request.content,
                        activity: request.activity,
                    },
                    cancellation,
                )
                .map(RuntimeResponse::from)
                .map_err(RuntimeError::from),
            Self::Pi(session) => session.run(request, cancellation),
        }
    }

    fn close(&mut self) -> Result<(), RuntimeError> {
        match self {
            Self::Acp(session) => session.close().map_err(RuntimeError::Acp),
            Self::Codex(session) => session.close().map_err(RuntimeError::from),
            Self::SharedCodex(session) => session.close().map_err(RuntimeError::from),
            Self::Pi(session) => session.process.close(),
        }
    }

    fn can_retry_after(&self, error: &RuntimeError) -> bool {
        match (self, error) {
            (Self::Acp(session), RuntimeError::Acp(error)) => session.can_retry_after(error),
            (_, RuntimeError::UnsupportedPrompt) => true,
            (Self::Codex(_), RuntimeError::Remote(_)) => true,
            (Self::SharedCodex(_), RuntimeError::Remote(_)) => true,
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
    #[serde(default)]
    shared_process: bool,
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
    /// Reasoning effort forwarded to every Codex `turn/start`
    /// (mirrors the host Codex adapter per-turn `effort`).
    #[serde(default)]
    effort: Option<String>,
    /// Codex thread approval policy. Absent means the native `"never"` default.
    #[serde(default)]
    approval_policy: Option<String>,
    /// Sandbox for model-executed shell commands (official SandboxMode).
    /// Absent = server default.
    #[serde(default)]
    sandbox: Option<String>,
    #[serde(default)]
    mcp_servers: Option<Value>,
    #[serde(default = "default_true")]
    ephemeral: bool,
    #[serde(default)]
    timeouts: NativeTimeouts,
    #[serde(default)]
    inherit_stderr: bool,
}

fn default_true() -> bool {
    true
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
        if matches!(kind, NativeKind::PiRpc) && self.effort.is_some() {
            return Err(
                "Pi RPC runtime does not accept reasoning effort; set the Pi model through args"
                    .into(),
            );
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
            effort: self.effort,
            mcp_servers,
            approval_policy: self.approval_policy.unwrap_or_else(|| "never".to_owned()),
            sandbox: self.sandbox,
            ephemeral: self.ephemeral,
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
            let shared_codex = if options.shared_process {
                Some(SharedCodexBackend::new())
            } else {
                None
            };
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
                    "codexAppServer" => {
                        let agent: NativeAgentOptions = serde_json::from_value(value)
                            .map_err(|error| format!("agent {index}: {error}"))?;
                        let native_config = agent
                            .into_config(NativeKind::CodexAppServer)
                            .map_err(|error| format!("agent {index}: {error}"))?;
                        if let Some(shared) = &shared_codex {
                            RuntimeConfig::SharedCodex(
                                shared.clone(),
                                native_config.to_codex_config(),
                            )
                        } else {
                            RuntimeConfig::Native(native_config)
                        }
                    }
                    "piRpc" => {
                        let agent: NativeAgentOptions = serde_json::from_value(value)
                            .map_err(|error| format!("agent {index}: {error}"))?;
                        RuntimeConfig::Native(
                            agent
                                .into_config(NativeKind::PiRpc)
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
