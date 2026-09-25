use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::Value;

use crate::{AcpConfig, AcpError};

pub(crate) struct Transport {
    child: Child,
    stdin: ChildStdin,
    incoming: Receiver<Result<Value, String>>,
    reader: Option<JoinHandle<()>>,
}

impl Transport {
    pub(crate) fn process_id(&self) -> u32 {
        self.child.id()
    }

    pub(crate) fn spawn(config: &AcpConfig) -> Result<Self, AcpError> {
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
        let (sender, incoming) = mpsc::channel();
        let reader = thread::Builder::new()
            .name("agentpools-acp-reader".into())
            .spawn(move || {
                let mut stdout = BufReader::new(stdout);
                loop {
                    let mut line = String::new();
                    match stdout.read_line(&mut line) {
                        Ok(0) => break,
                        Ok(_) if line.trim().is_empty() => continue,
                        Ok(_) => {
                            let message = serde_json::from_str::<Value>(&line)
                                .map_err(|error| format!("invalid ACP JSON: {error}"));
                            if sender.send(message).is_err() {
                                break;
                            }
                        }
                        Err(error) => {
                            let _ = sender.send(Err(format!("ACP stdout read failed: {error}")));
                            break;
                        }
                    }
                }
            });
        let reader = match reader {
            Ok(reader) => reader,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(AcpError::Spawn(error));
            }
        };
        Ok(Self {
            child,
            stdin,
            incoming,
            reader: Some(reader),
        })
    }

    pub(crate) fn send(&mut self, message: &Value) -> Result<(), AcpError> {
        serde_json::to_writer(&mut self.stdin, message)
            .map_err(|error| AcpError::Protocol(format!("ACP request encoding failed: {error}")))?;
        self.stdin.write_all(b"\n").map_err(AcpError::Io)?;
        self.stdin.flush().map_err(AcpError::Io)
    }

    /// `None` means only that no complete message arrived within `timeout`.
    pub(crate) fn receive(&self, timeout: Duration) -> Result<Option<Value>, AcpError> {
        match self.incoming.recv_timeout(timeout) {
            Ok(Ok(message)) => Ok(Some(message)),
            Ok(Err(error)) => Err(AcpError::Protocol(error)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => {
                Err(AcpError::Protocol("ACP stdout closed".to_string()))
            }
        }
    }

    pub(crate) fn terminate(&mut self) -> Result<(), AcpError> {
        if self.child.try_wait().map_err(AcpError::Io)?.is_none() {
            match self.child.kill() {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::InvalidInput => {}
                Err(error) => return Err(AcpError::Io(error)),
            }
        }
        self.child.wait().map_err(AcpError::Io)?;
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        Ok(())
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        let _ = self.terminate();
    }
}
