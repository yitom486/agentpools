use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use agentpools::CancellationToken;
use serde_json::Value;

use crate::{NativeConfig, RuntimeError};

pub(crate) struct JsonProcess {
    child: Child,
    stdin: Option<ChildStdin>,
    incoming: Receiver<Result<Value, String>>,
    pending: VecDeque<Value>,
}

impl JsonProcess {
    pub(crate) fn spawn(config: &NativeConfig) -> Result<Self, RuntimeError> {
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
        let mut child = command.spawn().map_err(RuntimeError::Spawn)?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| RuntimeError::Protocol("missing child stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| RuntimeError::Protocol("missing child stdout".into()))?;
        let (sender, incoming) = mpsc::channel();
        thread::Builder::new()
            .name("agentpools-runtime-reader".into())
            .spawn(move || {
                let mut reader = BufReader::new(stdout);
                let mut line = Vec::new();
                loop {
                    line.clear();
                    match reader.read_until(b'\n', &mut line) {
                        Ok(0) => break,
                        Ok(_) => {
                            let parsed = serde_json::from_slice::<Value>(&line)
                                .map_err(|error| format!("invalid JSON record: {error}"));
                            if sender.send(parsed).is_err() {
                                break;
                            }
                        }
                        Err(error) => {
                            let _ = sender.send(Err(format!("reading child stdout: {error}")));
                            break;
                        }
                    }
                }
            })
            .map_err(RuntimeError::Spawn)?;
        Ok(Self {
            child,
            stdin: Some(stdin),
            incoming,
            pending: VecDeque::new(),
        })
    }

    pub(crate) fn send(&mut self, message: &Value) -> Result<(), RuntimeError> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| RuntimeError::Protocol("runtime stdin is closed".into()))?;
        serde_json::to_writer(&mut *stdin, message)
            .map_err(|error| RuntimeError::Protocol(error.to_string()))?;
        stdin.write_all(b"\n").map_err(RuntimeError::Io)?;
        stdin.flush().map_err(RuntimeError::Io)
    }

    pub(crate) fn receive(
        &mut self,
        deadline: Instant,
        cancellation: Option<&CancellationToken>,
    ) -> Result<Value, RuntimeError> {
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            return Err(RuntimeError::Cancelled);
        }
        if let Some(message) = self.pending.pop_front() {
            return Ok(message);
        }
        self.receive_fresh(deadline, cancellation)
    }

    fn receive_fresh(
        &mut self,
        deadline: Instant,
        cancellation: Option<&CancellationToken>,
    ) -> Result<Value, RuntimeError> {
        loop {
            if cancellation.is_some_and(CancellationToken::is_cancelled) {
                return Err(RuntimeError::Cancelled);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(RuntimeError::Timeout);
            }
            match self
                .incoming
                .recv_timeout(remaining.min(Duration::from_millis(50)))
            {
                Ok(Ok(message)) => return Ok(message),
                Ok(Err(error)) => return Err(RuntimeError::Protocol(error)),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(RuntimeError::Protocol(
                        "runtime process closed stdout".into(),
                    ));
                }
            }
        }
    }

    pub(crate) fn response(
        &mut self,
        id: u64,
        deadline: Instant,
        cancellation: Option<&CancellationToken>,
    ) -> Result<Value, RuntimeError> {
        loop {
            let message = self.receive_fresh(deadline, cancellation)?;
            if message.get("id").and_then(Value::as_u64) == Some(id) {
                if let Some(error) = message.get("error") {
                    return Err(RuntimeError::Remote(error.to_string()));
                }
                return message
                    .get("result")
                    .cloned()
                    .ok_or_else(|| RuntimeError::Protocol("response has no result".into()));
            }
            self.pending.push_back(message);
            // Process notifications in arrival order after the matching reply.
            // Avoid repeatedly reading the same pending item while searching.
            let waiting = self.pending.len();
            if waiting > 4096 {
                return Err(RuntimeError::Protocol(
                    "too many pending runtime events".into(),
                ));
            }
        }
    }

    pub(crate) fn next_event(
        &mut self,
        deadline: Instant,
        cancellation: &CancellationToken,
    ) -> Result<Value, RuntimeError> {
        self.receive(deadline, Some(cancellation))
    }

    pub(crate) fn close(&mut self) -> Result<(), RuntimeError> {
        self.stdin.take();
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if self.child.try_wait().map_err(RuntimeError::Io)?.is_some() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                self.child.kill().map_err(RuntimeError::Io)?;
                self.child.wait().map_err(RuntimeError::Io)?;
                return Ok(());
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for JsonProcess {
    fn drop(&mut self) {
        let _ = self.close();
    }
}
