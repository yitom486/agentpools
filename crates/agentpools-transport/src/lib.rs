//! Cross-platform stdio child process management and JSON stream parsing.

use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::fmt;
use std::io::{self, BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use agentpools::CancellationToken;
use serde_json::Value;

#[derive(Debug)]
pub enum TransportError {
    Spawn(io::Error),
    Io(io::Error),
    Protocol(String),
    Remote(String),
    Timeout,
    Cancelled,
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn(err) => write!(f, "cannot spawn process: {err}"),
            Self::Io(err) => write!(f, "transport I/O failed: {err}"),
            Self::Protocol(err) => write!(f, "protocol error: {err}"),
            Self::Remote(err) => write!(f, "remote error: {err}"),
            Self::Timeout => write!(f, "operation timed out"),
            Self::Cancelled => write!(f, "operation cancelled"),
        }
    }
}

impl std::error::Error for TransportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Spawn(err) | Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<io::Error> for TransportError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

/// Cross-platform process launch configuration.
#[derive(Clone, Debug)]
pub struct ProcessConfig {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub env: HashMap<OsString, OsString>,
    pub cwd: PathBuf,
    pub inherit_stderr: bool,
}

impl ProcessConfig {
    pub fn new(program: impl Into<PathBuf>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: HashMap::new(),
            cwd: cwd.into(),
            inherit_stderr: false,
        }
    }
}

/// Safely terminates a child process across Windows, Linux, and macOS.
///
/// 1. Checks if the child already exited via `try_wait()`.
/// 2. If not, waits up to `grace_period` for clean exit.
/// 3. If still running, forcefully terminates the child, handling platform-specific error quirks.
/// 4. Waits for the child and joins the reader thread.
pub fn terminate_child(
    child: &mut Child,
    reader: Option<JoinHandle<()>>,
    grace_period: Duration,
) -> io::Result<()> {
    if child.try_wait()?.is_none() {
        let deadline = Instant::now() + grace_period;
        let mut exited = false;
        while Instant::now() < deadline {
            if child.try_wait()?.is_some() {
                exited = true;
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        if !exited {
            match child.kill() {
                Ok(()) => {}
                // On Windows, killing an already exited process may return InvalidInput or PermissionDenied
                Err(err) if err.kind() == io::ErrorKind::InvalidInput => {}
                Err(err) if err.kind() == io::ErrorKind::PermissionDenied => {}
                Err(err) => return Err(err),
            }
        }
    }
    let _ = child.wait();
    if let Some(handle) = reader {
        let _ = handle.join();
    }
    Ok(())
}

/// Dedicated JSON-RPC/line-delimited stdio transport for a single child process.
pub struct JsonProcess {
    child: Child,
    stdin: Option<ChildStdin>,
    incoming: Receiver<Result<Value, String>>,
    pending: VecDeque<Value>,
    reader: Option<JoinHandle<()>>,
    process_id: u32,
}

impl JsonProcess {
    pub fn spawn(config: &ProcessConfig) -> Result<Self, TransportError> {
        let mut command = Command::new(&config.program);
        command
            .args(&config.args)
            .current_dir(&config.cwd)
            .envs(&config.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(if config.inherit_stderr {
                Stdio::inherit()
            } else {
                Stdio::null()
            });

        let mut child = command.spawn().map_err(TransportError::Spawn)?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| TransportError::Protocol("missing child stdin pipe".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| TransportError::Protocol("missing child stdout pipe".into()))?;
        let process_id = child.id();

        let (sender, incoming) = mpsc::channel();
        let reader = thread::Builder::new()
            .name("agentpools-transport-reader".into())
            .spawn(move || {
                let mut reader = BufReader::new(stdout);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line) {
                        Ok(0) => break,
                        Ok(_) => {
                            let trimmed = line.trim();
                            if trimmed.is_empty() {
                                continue;
                            }
                            let parsed = serde_json::from_str::<Value>(trimmed)
                                .map_err(|err| format!("invalid JSON record: {err}"));
                            if sender.send(parsed).is_err() {
                                break;
                            }
                        }
                        Err(err) => {
                            let _ = sender.send(Err(format!("reading child stdout: {err}")));
                            break;
                        }
                    }
                }
            })
            .map_err(TransportError::Spawn)?;

        Ok(Self {
            child,
            stdin: Some(stdin),
            incoming,
            pending: VecDeque::new(),
            reader: Some(reader),
            process_id,
        })
    }

    pub fn process_id(&self) -> u32 {
        self.process_id
    }

    pub fn send(&mut self, message: &Value) -> Result<(), TransportError> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| TransportError::Protocol("transport stdin is closed".into()))?;
        serde_json::to_writer(&mut *stdin, message)
            .map_err(|err| TransportError::Protocol(format!("encoding request failed: {err}")))?;
        stdin.write_all(b"\n").map_err(TransportError::Io)?;
        stdin.flush().map_err(TransportError::Io)
    }

    /// Read any incoming message within the given duration.
    /// Returns `None` if timeout expires without a message.
    pub fn receive(&mut self, timeout: Duration) -> Result<Option<Value>, TransportError> {
        if let Some(msg) = self.pending.pop_front() {
            return Ok(Some(msg));
        }
        match self.incoming.recv_timeout(timeout) {
            Ok(Ok(msg)) => Ok(Some(msg)),
            Ok(Err(err)) => Err(TransportError::Protocol(err)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => {
                Err(TransportError::Protocol("process stdout closed".into()))
            }
        }
    }

    /// Read the next event, prioritizing pending queued items, with cancellation support.
    pub fn next_event(
        &mut self,
        deadline: Instant,
        cancellation: &CancellationToken,
    ) -> Result<Value, TransportError> {
        if cancellation.is_cancelled() {
            return Err(TransportError::Cancelled);
        }
        if let Some(msg) = self.pending.pop_front() {
            return Ok(msg);
        }
        self.receive_fresh(deadline, Some(cancellation))
    }

    /// Blocks until a new message arrives from the child stdout, respecting deadline and cancellation.
    pub fn receive_fresh(
        &mut self,
        deadline: Instant,
        cancellation: Option<&CancellationToken>,
    ) -> Result<Value, TransportError> {
        loop {
            if cancellation.is_some_and(CancellationToken::is_cancelled) {
                return Err(TransportError::Cancelled);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(TransportError::Timeout);
            }
            match self
                .incoming
                .recv_timeout(remaining.min(Duration::from_millis(50)))
            {
                Ok(Ok(msg)) => return Ok(msg),
                Ok(Err(err)) => return Err(TransportError::Protocol(err)),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(TransportError::Protocol("process stdout closed".into()));
                }
            }
        }
    }

    /// Await a response matching `id`, queuing intermediate notifications.
    pub fn response(
        &mut self,
        id: u64,
        deadline: Instant,
        cancellation: Option<&CancellationToken>,
    ) -> Result<Value, TransportError> {
        loop {
            let message = self.receive_fresh(deadline, cancellation)?;
            if message.get("id").and_then(Value::as_u64) == Some(id) {
                if let Some(error) = message.get("error") {
                    return Err(TransportError::Remote(error.to_string()));
                }
                return message
                    .get("result")
                    .cloned()
                    .ok_or_else(|| TransportError::Protocol("response has no result".into()));
            }
            self.pending.push_back(message);
            if self.pending.len() > 4096 {
                return Err(TransportError::Protocol("too many pending events".into()));
            }
        }
    }

    /// Closes stdin (sending EOF to process), then waits up to `grace_period` before force killing.
    pub fn close(&mut self) -> Result<(), TransportError> {
        self.stdin.take();
        terminate_child(&mut self.child, self.reader.take(), Duration::from_millis(500))
            .map_err(TransportError::Io)
    }

    /// Terminate immediately with custom grace period.
    pub fn terminate(&mut self, grace_period: Duration) -> Result<(), TransportError> {
        self.stdin.take();
        terminate_child(&mut self.child, self.reader.take(), grace_period)
            .map_err(TransportError::Io)
    }
}

impl Drop for JsonProcess {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_process_config_creation() {
        let config = ProcessConfig::new("node", ".");
        assert_eq!(config.program, PathBuf::from("node"));
        assert_eq!(config.cwd, PathBuf::from("."));
        assert!(!config.inherit_stderr);
        assert!(config.args.is_empty());
        assert!(config.env.is_empty());
    }

    #[test]
    fn test_transport_error_display() {
        let err = TransportError::Protocol("test error".into());
        assert!(err.to_string().contains("test error"));
        let err = TransportError::Cancelled;
        assert_eq!(err.to_string(), "operation cancelled");
    }
}

