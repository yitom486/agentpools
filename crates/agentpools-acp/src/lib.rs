//! A stdio ACP v1 adapter for [`agentpools`].
//!
//! By default, each pool worker owns one Agent process and one ACP session.
//! [`SharedAcpBackend`] is an explicit alternative for agents that multiplex
//! independent ACP sessions over one process. The process may be a Node.js
//! package entry point, an `npx` launcher, or a native ACP executable. MCP
//! server definitions are passed to `session/new` unchanged.

mod interop;
mod shared_process;
mod transport;

pub use interop::{
    ACP_POOL_API_VERSION, AcpAgentOptions, AcpPoolBuildError, AcpPoolOptions, AcpTimeoutOptions,
};
pub use shared_process::{SharedAcpBackend, SharedAcpSession};

use std::collections::HashMap;
use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agentpools::{AgentBackend, AgentSession, CancellationToken};
use serde_json::{Value, json};
use transport::Transport;

/// Client-side requests such as permission, filesystem, and terminal calls.
/// Implementations must enforce their own authorization and time limits.
pub trait HostRequestHandler: Send + Sync {
    fn handle(&self, method: &str, params: &Value) -> Result<Value, AcpError>;
}

/// Launch and session options for a single ACP worker.
pub struct AcpConfig {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub env: HashMap<OsString, OsString>,
    /// Absolute working directory sent to `session/new`.
    pub cwd: PathBuf,
    /// Opaque caller-provided ACP MCP server objects.
    pub mcp_servers: Vec<Value>,
    /// Explicit ACP authentication method, if this Agent needs one.
    pub auth_method: Option<String>,
    /// Requested ACP model config option. The adapter verifies the selected
    /// value before returning a session, rather than silently using a default.
    pub model: Option<String>,
    /// Advertise only client-side capabilities actually implemented by
    /// `host_handler`. Defaults to `{}`, which exposes no host tools.
    pub client_capabilities: Value,
    pub host_handler: Option<Arc<dyn HostRequestHandler>>,
    pub handshake_timeout: Duration,
    pub prompt_timeout: Duration,
    pub close_timeout: Duration,
    pub inherit_stderr: bool,
    pub ephemeral: bool,
}

impl AcpConfig {
    pub fn new(program: impl Into<PathBuf>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: HashMap::new(),
            cwd: cwd.into(),
            mcp_servers: Vec::new(),
            auth_method: None,
            model: None,
            client_capabilities: json!({}),
            host_handler: None,
            handshake_timeout: Duration::from_secs(30),
            prompt_timeout: Duration::from_secs(300),
            close_timeout: Duration::from_secs(3),
            inherit_stderr: false,
            ephemeral: true,
        }
    }

    pub fn with_ephemeral(mut self, ephemeral: bool) -> Self {
        self.ephemeral = ephemeral;
        self
    }
}

#[derive(Debug)]
pub enum AcpError {
    InvalidConfig(&'static str),
    Spawn(io::Error),
    Io(io::Error),
    Protocol(String),
    Remote { code: i64, message: String },
    Timeout(&'static str),
    Cancelled,
    NoOutput,
}

impl fmt::Display for AcpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig(message) => write!(f, "invalid ACP configuration: {message}"),
            Self::Spawn(error) => write!(f, "cannot start ACP agent: {error}"),
            Self::Io(error) => write!(f, "ACP I/O failed: {error}"),
            Self::Protocol(message) => write!(f, "ACP protocol failed: {message}"),
            Self::Remote { code, message } => write!(f, "ACP agent error {code}: {message}"),
            Self::Timeout(stage) => write!(f, "ACP {stage} timed out"),
            Self::Cancelled => write!(f, "ACP prompt cancelled"),
            Self::NoOutput => write!(f, "ACP agent returned no text"),
        }
    }
}

impl std::error::Error for AcpError {}

/// An ACP prompt. Use [`AcpPrompt::text`] for ordinary text; other content
/// blocks can be passed exactly as required by the Agent's ACP version.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AcpPrompt {
    pub content: Vec<Value>,
}

impl AcpPrompt {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![json!({ "type": "text", "text": text.into() })],
        }
    }

    /// Prepend a caller-provided feedback block for an explicit same-session
    /// retry. The scheduler only uses this request if the ACP adapter confirms
    /// that the previous remote error left the session synchronized.
    pub fn with_retry_feedback(&self, feedback: impl Into<String>) -> Self {
        let mut content = Vec::with_capacity(self.content.len() + 1);
        content.push(json!({ "type": "text", "text": feedback.into() }));
        content.extend(self.content.iter().cloned());
        Self { content }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AcpResponse {
    pub text: String,
    pub stop_reason: String,
}

#[derive(Default, Clone, Copy)]
pub struct AcpBackend;

impl AgentBackend for AcpBackend {
    type Config = AcpConfig;
    type Request = AcpPrompt;
    type Response = AcpResponse;
    type Error = AcpError;
    type Session = AcpSession;

    fn open(&self, config: &Self::Config) -> Result<Self::Session, Self::Error> {
        AcpSession::open(config)
    }
}

/// One live process and ACP session, owned by a single pool worker.
pub struct AcpSession {
    transport: Transport,
    session_id: String,
    supports_close: bool,
    ephemeral: bool,
    handler: Option<Arc<dyn HostRequestHandler>>,
    prompt_timeout: Duration,
    close_timeout: Duration,
    next_id: u64,
    selected_model: Option<String>,
    closed: bool,
}

impl AcpSession {
    fn open(config: &AcpConfig) -> Result<Self, AcpError> {
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
        let transport = Transport::spawn(config)?;
        let mut session = Self {
            transport,
            session_id: String::new(),
            supports_close: false,
            ephemeral: config.ephemeral,
            handler: config.host_handler.clone(),
            prompt_timeout: config.prompt_timeout,
            close_timeout: config.close_timeout,
            next_id: 1,
            selected_model: None,
            closed: false,
        };
        let initialized = session.call(
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
        )?;
        let version = initialized.get("protocolVersion").and_then(Value::as_u64);
        if version != Some(1) {
            return Err(AcpError::Protocol(format!(
                "unsupported ACP version: {version:?}"
            )));
        }
        session.supports_close = initialized
            .pointer("/agentCapabilities/sessionCapabilities/close")
            .or_else(|| initialized.pointer("/agentCapabilities/session/close"))
            .is_some_and(|value| !value.is_null() && value != false);
        if let Some(method) = &config.auth_method {
            session.call(
                "authenticate",
                json!({ "methodId": method }),
                config.handshake_timeout,
            )?;
        }
        let mut session_new_params = json!({
            "cwd": config.cwd.to_string_lossy(),
            "mcpServers": config.mcp_servers
        });
        if config.ephemeral {
            session_new_params["ephemeral"] = json!(true);
        }
        let created = session.call(
            "session/new",
            session_new_params,
            config.handshake_timeout,
        )?;
        session.session_id = created
            .get("sessionId")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| AcpError::Protocol("session/new omitted sessionId".into()))?
            .to_string();
        session.selected_model = selected_model(&created).map(str::to_string);
        if let Some(requested_model) = &config.model {
            let response = session.call(
                "session/set_config_option",
                json!({
                    "sessionId": session.session_id,
                    "configId": "model",
                    "value": requested_model
                }),
                config.handshake_timeout,
            )?;
            let actual = selected_model(&response).ok_or_else(|| {
                AcpError::Protocol("model config response omitted selected model".into())
            })?;
            if actual != requested_model {
                return Err(AcpError::Protocol(format!(
                    "requested model {requested_model}, Agent selected {actual}"
                )));
            }
            session.selected_model = Some(actual.to_string());
        }
        Ok(session)
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Operating system process ID of this worker's ACP Agent.
    pub fn process_id(&self) -> u32 {
        self.transport.process_id()
    }

    pub fn selected_model(&self) -> Option<&str> {
        self.selected_model.as_deref()
    }

    fn send_request(&mut self, method: &str, params: Value) -> Result<u64, AcpError> {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        self.transport.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        }))?;
        Ok(id)
    }

    fn call(
        &mut self,
        method: &'static str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, AcpError> {
        let id = self.send_request(method, params)?;
        let deadline = Instant::now() + timeout;
        loop {
            let Some(message) = self.receive_until(deadline, method)? else {
                continue;
            };
            if response_id(&message) == Some(id) {
                return response_result(&message);
            }
            self.handle_side_message(&message)?;
        }
    }

    fn receive_until(
        &mut self,
        deadline: Instant,
        stage: &'static str,
    ) -> Result<Option<Value>, AcpError> {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or(AcpError::Timeout(stage))?;
        self.transport
            .receive(remaining.min(Duration::from_millis(50)))
    }

    fn handle_side_message(&mut self, message: &Value) -> Result<(), AcpError> {
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            return Ok(());
        };
        let Some(id) = message.get("id") else {
            return Ok(());
        };
        let params = message.get("params").unwrap_or(&Value::Null);
        let result = if let Some(handler) = &self.handler {
            handler.handle(method, params)
        } else if method == "session/request_permission" {
            Ok(json!({ "outcome": { "outcome": "cancelled" } }))
        } else {
            Err(AcpError::Protocol(format!(
                "client does not implement {method}"
            )))
        };
        let reply = match result {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(error) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": error.to_string() }
            }),
        };
        self.transport.send(&reply)
    }

    fn cancel_prompt(&mut self) -> Result<(), AcpError> {
        self.transport.send(&json!({
            "jsonrpc": "2.0",
            "method": "session/cancel",
            "params": { "sessionId": self.session_id }
        }))
    }
}

impl AgentSession<AcpPrompt, AcpResponse, AcpError> for AcpSession {
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
        let id = self.send_request(
            "session/prompt",
            json!({ "sessionId": self.session_id, "prompt": request.content }),
        )?;
        let prompt_deadline = Instant::now() + self.prompt_timeout;
        let mut cancel_deadline = None;
        let mut text = String::new();
        loop {
            if cancellation.is_cancelled() && cancel_deadline.is_none() {
                self.cancel_prompt()?;
                cancel_deadline = Some(Instant::now() + self.close_timeout);
            }
            let deadline = cancel_deadline.unwrap_or(prompt_deadline);
            let message = match self.receive_until(deadline, "prompt") {
                Ok(message) => message,
                Err(AcpError::Timeout(_)) if cancel_deadline.is_some() => {
                    return Err(AcpError::Cancelled);
                }
                Err(error) => return Err(error),
            };
            let Some(message) = message else { continue };
            if response_id(&message) == Some(id) {
                let result = response_result(&message)?;
                if cancel_deadline.is_some() {
                    return Err(AcpError::Cancelled);
                }
                if text.is_empty() {
                    return Err(AcpError::NoOutput);
                }
                return Ok(AcpResponse {
                    text,
                    stop_reason: result
                        .get("stopReason")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_string(),
                });
            }
            if message.get("method").and_then(Value::as_str) == Some("session/update")
                && message.pointer("/params/sessionId").and_then(Value::as_str)
                    == Some(self.session_id.as_str())
            {
                append_text_update(&mut text, &message);
            }
            self.handle_side_message(&message)?;
        }
    }

    fn close(&mut self) -> Result<(), AcpError> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        let delete_result = if self.ephemeral && !self.session_id.is_empty() {
            let _ = self.call(
                "session/delete",
                json!({ "sessionId": self.session_id }),
                self.close_timeout,
            );
            Ok(())
        } else {
            Ok(())
        };
        let close_result = if self.supports_close && !self.session_id.is_empty() {
            self.call(
                "session/close",
                json!({ "sessionId": self.session_id }),
                self.close_timeout,
            )
            .map(|_| ())
        } else {
            Ok(())
        };
        let process_result = self.transport.terminate();
        delete_result.and(close_result).and(process_result)
    }

    fn can_retry_after(&self, error: &AcpError) -> bool {
        // The matching JSON-RPC response has been received, so a remote
        // application error leaves the transport synchronized. I/O, timeout,
        // cancellation and malformed protocol messages have uncertain state.
        !self.closed && matches!(error, AcpError::Remote { .. })
    }
}

impl Drop for AcpSession {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

fn response_id(message: &Value) -> Option<u64> {
    message.get("id").and_then(Value::as_u64)
}

fn selected_model(response: &Value) -> Option<&str> {
    response
        .get("configOptions")?
        .as_array()?
        .iter()
        .find(|option| option.get("id").and_then(Value::as_str) == Some("model"))?
        .get("currentValue")?
        .as_str()
}

fn response_result(message: &Value) -> Result<Value, AcpError> {
    if let Some(error) = message.get("error") {
        return Err(AcpError::Remote {
            code: error.get("code").and_then(Value::as_i64).unwrap_or(-32000),
            message: error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("agent rejected request")
                .to_string(),
        });
    }
    message
        .get("result")
        .cloned()
        .ok_or_else(|| AcpError::Protocol("response omitted result".into()))
}

fn append_text_update(text: &mut String, message: &Value) {
    let update = &message["params"]["update"];
    let Some(fragment) = update
        .pointer("/content/text")
        .and_then(Value::as_str)
        .or_else(|| {
            update
                .pointer("/message/content/text")
                .and_then(Value::as_str)
        })
    else {
        return;
    };
    match update.get("sessionUpdate").and_then(Value::as_str) {
        Some("agent_message_chunk") => text.push_str(fragment),
        Some("agent_message") => {
            text.clear();
            text.push_str(fragment);
        }
        _ => {}
    }
}
