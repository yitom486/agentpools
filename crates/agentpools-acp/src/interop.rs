//! Versioned, JSON-serializable configuration for language bindings.
//!
//! The core [`agentpools`] API intentionally uses Rust traits and associated
//! types. This module provides a concrete ACP-backed surface that Node and
//! Python bindings can construct without exposing those Rust-only types.

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use agentpools::{AgentPool, BuildError};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    AcpBackend, AcpConfig, SharedAcpBackend, shared_process::shared_process_profile_mismatch,
};

/// Version of the language-neutral ACP pool configuration contract.
pub const ACP_POOL_API_VERSION: u16 = 1;

/// Serializable configuration for one ACP worker process.
///
/// `mcp_servers` is passed through to ACP `session/new` as supplied. This
/// configuration deliberately contains no host callback: client-side ACP
/// requests such as filesystem or terminal access remain opt-in adapter
/// capabilities and are not advertised by language bindings in API v1.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AcpAgentOptions {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub cwd: PathBuf,
    #[serde(default)]
    pub mcp_servers: Vec<Value>,
    #[serde(default)]
    pub auth_method: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub timeouts: AcpTimeoutOptions,
    #[serde(default)]
    pub inherit_stderr: bool,
}

/// ACP lifecycle timeout values, in milliseconds, for JSON interoperability.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AcpTimeoutOptions {
    #[serde(default = "default_handshake_timeout_ms")]
    pub handshake_ms: u64,
    #[serde(default = "default_prompt_timeout_ms")]
    pub prompt_ms: u64,
    #[serde(default = "default_close_timeout_ms")]
    pub close_ms: u64,
}

impl Default for AcpTimeoutOptions {
    fn default() -> Self {
        Self {
            handshake_ms: default_handshake_timeout_ms(),
            prompt_ms: default_prompt_timeout_ms(),
            close_ms: default_close_timeout_ms(),
        }
    }
}

/// Concrete language-neutral configuration for an ACP-backed pool.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AcpPoolOptions {
    pub api_version: u16,
    pub agents: Vec<AcpAgentOptions>,
    #[serde(default = "default_max_queued")]
    pub max_queued: usize,
}

impl AcpPoolOptions {
    /// Create a pool with one ACP process/session slot per configured agent.
    pub fn build(self) -> Result<AgentPool<AcpBackend>, AcpPoolBuildError> {
        let (configs, max_queued) = self.into_configs()?;
        let agents = configs
            .into_iter()
            .map(|config| (AcpBackend, config))
            .collect();
        AgentPool::with_agents(agents, max_queued).map_err(AcpPoolBuildError::Pool)
    }

    /// Create a pool whose agent sessions share one ACP process. The process
    /// launch, authentication, client capabilities, and environment must match
    /// across agents; cwd, MCP servers, and model remain per-session options.
    pub fn build_shared(self) -> Result<AgentPool<SharedAcpBackend>, AcpPoolBuildError> {
        let (configs, max_queued) = self.into_configs()?;
        if let Some(index) = shared_process_profile_mismatch(&configs) {
            return Err(AcpPoolBuildError::InvalidAgent {
                index,
                reason: "shared ACP agents must use the same process, auth, and client capability settings",
            });
        }
        let backend = SharedAcpBackend::new();
        let agents = configs
            .into_iter()
            .map(|config| (backend.clone(), config))
            .collect();
        AgentPool::with_agents(agents, max_queued).map_err(AcpPoolBuildError::Pool)
    }

    fn into_configs(self) -> Result<(Vec<AcpConfig>, usize), AcpPoolBuildError> {
        if self.api_version != ACP_POOL_API_VERSION {
            return Err(AcpPoolBuildError::UnsupportedApiVersion(self.api_version));
        }
        if self.agents.is_empty() {
            return Err(AcpPoolBuildError::NoAgents);
        }
        if self.max_queued == 0 {
            return Err(AcpPoolBuildError::InvalidQueueCapacity);
        }

        let mut configs = Vec::with_capacity(self.agents.len());
        for (index, options) in self.agents.into_iter().enumerate() {
            configs.push(options.into_config(index)?);
        }
        Ok((configs, self.max_queued))
    }
}

impl AcpAgentOptions {
    fn into_config(self, index: usize) -> Result<AcpConfig, AcpPoolBuildError> {
        if self.program.trim().is_empty() {
            return Err(AcpPoolBuildError::InvalidAgent {
                index,
                reason: "program must not be empty",
            });
        }
        if !self.cwd.is_absolute() {
            return Err(AcpPoolBuildError::InvalidAgent {
                index,
                reason: "cwd must be an absolute path",
            });
        }
        if self.timeouts.handshake_ms == 0
            || self.timeouts.prompt_ms == 0
            || self.timeouts.close_ms == 0
        {
            return Err(AcpPoolBuildError::InvalidAgent {
                index,
                reason: "timeouts must be greater than zero milliseconds",
            });
        }

        let mut config = AcpConfig::new(self.program, self.cwd);
        config.args = self.args.into_iter().map(OsString::from).collect();
        config.env = self
            .env
            .into_iter()
            .map(|(key, value)| (OsString::from(key), OsString::from(value)))
            .collect::<HashMap<_, _>>();
        config.mcp_servers = self.mcp_servers;
        config.auth_method = self.auth_method;
        config.model = self.model;
        config.handshake_timeout = Duration::from_millis(self.timeouts.handshake_ms);
        config.prompt_timeout = Duration::from_millis(self.timeouts.prompt_ms);
        config.close_timeout = Duration::from_millis(self.timeouts.close_ms);
        config.inherit_stderr = self.inherit_stderr;
        Ok(config)
    }
}

/// Error returned while constructing a pool from the language-neutral config.
#[derive(Debug)]
pub enum AcpPoolBuildError {
    UnsupportedApiVersion(u16),
    NoAgents,
    InvalidQueueCapacity,
    InvalidAgent { index: usize, reason: &'static str },
    Pool(BuildError),
}

impl fmt::Display for AcpPoolBuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedApiVersion(version) => write!(
                f,
                "unsupported ACP pool API version {version}; expected {ACP_POOL_API_VERSION}"
            ),
            Self::NoAgents => write!(f, "at least one ACP agent must be configured"),
            Self::InvalidQueueCapacity => write!(f, "maxQueued must be greater than zero"),
            Self::InvalidAgent { index, reason } => {
                write!(f, "invalid ACP agent at index {index}: {reason}")
            }
            Self::Pool(error) => write!(f, "cannot create ACP agent pool: {error}"),
        }
    }
}

impl std::error::Error for AcpPoolBuildError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Pool(error) => Some(error),
            _ => None,
        }
    }
}

fn default_max_queued() -> usize {
    128
}

fn default_handshake_timeout_ms() -> u64 {
    30_000
}

fn default_prompt_timeout_ms() -> u64 {
    300_000
}

fn default_close_timeout_ms() -> u64 {
    3_000
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn config_uses_camel_case_and_forwards_mcp_json_unchanged() {
        let options = AcpPoolOptions {
            api_version: ACP_POOL_API_VERSION,
            agents: vec![AcpAgentOptions {
                program: "codex-acp".into(),
                args: vec!["--stdio".into()],
                env: BTreeMap::from([("MODE".into(), "test".into())]),
                cwd: PathBuf::from("C:\\workspace"),
                mcp_servers: vec![json!({
                    "name": "caller-tool",
                    "command": "tool-server",
                    "args": ["--readonly"],
                    "env": [{"name": "LEVEL", "value": "1"}]
                })],
                auth_method: Some("oauth".into()),
                model: Some("gpt-6-luna".into()),
                timeouts: AcpTimeoutOptions::default(),
                inherit_stderr: false,
            }],
            max_queued: 12,
        };

        let encoded = serde_json::to_value(&options).unwrap();
        assert_eq!(encoded["apiVersion"], 1);
        assert_eq!(encoded["maxQueued"], 12);
        assert_eq!(encoded["agents"][0]["mcpServers"][0]["name"], "caller-tool");
        assert_eq!(encoded["agents"][0]["model"], "gpt-6-luna");

        let decoded: AcpPoolOptions = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded.agents[0].mcp_servers, options.agents[0].mcp_servers);
    }

    #[test]
    fn config_rejects_unknown_fields_and_invalid_versions() {
        let unknown = json!({
            "apiVersion": 1,
            "agents": [],
            "maxQueued": 1,
            "surprise": true
        });
        assert!(serde_json::from_value::<AcpPoolOptions>(unknown).is_err());

        let unsupported = AcpPoolOptions {
            api_version: 99,
            agents: vec![],
            max_queued: 1,
        }
        .build();
        assert!(matches!(
            unsupported,
            Err(AcpPoolBuildError::UnsupportedApiVersion(99))
        ));
    }

    #[test]
    fn prompt_and_response_are_serializable_language_types() {
        let prompt = crate::AcpPrompt::text("add 2 and 3");
        let prompt_json = serde_json::to_value(&prompt).unwrap();
        assert_eq!(prompt_json["content"][0]["text"], "add 2 and 3");

        let retry = prompt.with_retry_feedback("attempt 1 failed");
        assert_eq!(retry.content[0]["text"], "attempt 1 failed");
        assert_eq!(retry.content[1]["text"], "add 2 and 3");

        let response = crate::AcpResponse {
            text: "5".into(),
            stop_reason: "end_turn".into(),
        };
        let response_json = serde_json::to_value(response).unwrap();
        assert_eq!(response_json["stopReason"], "end_turn");
    }
}
