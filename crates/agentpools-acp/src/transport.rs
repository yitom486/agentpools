use std::time::Duration;

use agentpools_transport::{JsonProcess, ProcessConfig, TransportError};
use serde_json::Value;

use crate::{AcpConfig, AcpError};

impl From<TransportError> for AcpError {
    fn from(err: TransportError) -> Self {
        match err {
            TransportError::Spawn(e) => AcpError::Spawn(e),
            TransportError::Io(e) => AcpError::Io(e),
            TransportError::Protocol(e) => AcpError::Protocol(e),
            TransportError::Remote(message) => AcpError::Remote {
                code: -32603,
                message,
            },
            TransportError::Timeout => AcpError::Timeout("transport timeout"),
            TransportError::Cancelled => AcpError::Cancelled,
        }
    }
}

pub(crate) struct Transport {
    inner: JsonProcess,
}

impl Transport {
    pub(crate) fn process_id(&self) -> u32 {
        self.inner.process_id()
    }

    pub(crate) fn spawn(config: &AcpConfig) -> Result<Self, AcpError> {
        let process_config = ProcessConfig {
            program: config.program.clone(),
            args: config.args.clone(),
            env: config.env.clone(),
            cwd: config.cwd.clone(),
            inherit_stderr: config.inherit_stderr,
        };
        let inner = JsonProcess::spawn(&process_config)?;
        Ok(Self { inner })
    }

    pub(crate) fn send(&mut self, message: &Value) -> Result<(), AcpError> {
        self.inner.send(message).map_err(AcpError::from)
    }

    pub(crate) fn receive(&mut self, timeout: Duration) -> Result<Option<Value>, AcpError> {
        self.inner.receive(timeout).map_err(AcpError::from)
    }

    pub(crate) fn terminate(&mut self) -> Result<(), AcpError> {
        self.inner.close().map_err(AcpError::from)
    }
}
