use std::ffi::OsString;
use std::path::PathBuf;

use agentpools_transport::{JsonProcess as TransportProcess, ProcessConfig, TransportError};

use crate::{NativeConfig, RuntimeError};

impl From<TransportError> for RuntimeError {
    fn from(err: TransportError) -> Self {
        match err {
            TransportError::Spawn(e) => RuntimeError::Spawn(e),
            TransportError::Io(e) => RuntimeError::Io(e),
            TransportError::Protocol(e) => RuntimeError::Protocol(e),
            TransportError::Remote(e) => RuntimeError::Remote(e),
            TransportError::Timeout => RuntimeError::Timeout,
            TransportError::Cancelled => RuntimeError::Cancelled,
        }
    }
}

#[allow(dead_code)]
pub(crate) struct JsonProcess {
    inner: TransportProcess,
}

#[allow(dead_code)]
impl JsonProcess {
    pub(crate) fn spawn(config: &NativeConfig) -> Result<Self, RuntimeError> {
        let process_config = ProcessConfig {
            program: PathBuf::from(&config.program),
            args: config.args.iter().map(OsString::from).collect(),
            env: config
                .env
                .iter()
                .map(|(k, v)| (OsString::from(k), OsString::from(v)))
                .collect(),
            cwd: config.cwd.clone(),
            inherit_stderr: config.inherit_stderr,
        };
        let inner = TransportProcess::spawn(&process_config)?;
        Ok(Self { inner })
    }

    pub(crate) fn send(&mut self, message: &serde_json::Value) -> Result<(), RuntimeError> {
        self.inner.send(message).map_err(RuntimeError::from)
    }

    pub(crate) fn receive(
        &mut self,
        deadline: std::time::Instant,
        cancellation: Option<&agentpools::CancellationToken>,
    ) -> Result<serde_json::Value, RuntimeError> {
        self.inner
            .receive_fresh(deadline, cancellation)
            .map_err(RuntimeError::from)
    }

    pub(crate) fn response(
        &mut self,
        id: u64,
        deadline: std::time::Instant,
        cancellation: Option<&agentpools::CancellationToken>,
    ) -> Result<serde_json::Value, RuntimeError> {
        self.inner
            .response(id, deadline, cancellation)
            .map_err(RuntimeError::from)
    }

    pub(crate) fn next_event(
        &mut self,
        deadline: std::time::Instant,
        cancellation: &agentpools::CancellationToken,
    ) -> Result<serde_json::Value, RuntimeError> {
        self.inner
            .next_event(deadline, cancellation)
            .map_err(RuntimeError::from)
    }

    pub(crate) fn close(&mut self) -> Result<(), RuntimeError> {
        self.inner.close().map_err(RuntimeError::from)
    }
}
