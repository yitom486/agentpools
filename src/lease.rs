use crate::{AcquireError, AgentBackend, CancellationToken, TaskError};
use std::sync::mpsc;
use std::time::Duration;

pub(crate) struct LeaseReady<B: AgentBackend> {
    pub(crate) agent_index: usize,
    pub(crate) commands: mpsc::Sender<LeaseCommand<B>>,
}

pub(crate) enum LeaseCommand<B: AgentBackend> {
    Ask {
        request: B::Request,
        cancellation: CancellationToken,
        result: mpsc::Sender<Result<B::Response, TaskError<B::Error>>>,
    },
    Release {
        completed: Option<mpsc::Sender<()>>,
    },
}

/// A queued request to reserve one worker. `wait` returns its exclusive lease.
/// Dropping this handle before `wait` cancels the pending reservation.
pub struct LeaseHandle<B: AgentBackend> {
    pub(crate) cancellation: Option<CancellationToken>,
    pub(crate) result: mpsc::Receiver<Result<LeaseReady<B>, AcquireError<B::Error>>>,
}

impl<B: AgentBackend> LeaseHandle<B> {
    pub fn cancel(&self) {
        if let Some(cancellation) = &self.cancellation {
            cancellation.cancel();
        }
    }

    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancellation
            .as_ref()
            .expect("lease handle has not been consumed")
            .clone()
    }

    pub fn wait(mut self) -> Result<SessionLease<B>, AcquireError<B::Error>> {
        loop {
            if self
                .cancellation
                .as_ref()
                .is_some_and(CancellationToken::is_cancelled)
            {
                return Err(AcquireError::Cancelled);
            }
            match self.result.recv_timeout(Duration::from_millis(25)) {
                Ok(result) => {
                    if self
                        .cancellation
                        .as_ref()
                        .is_some_and(CancellationToken::is_cancelled)
                    {
                        return Err(AcquireError::Cancelled);
                    }
                    self.cancellation.take();
                    return result.map(|ready| SessionLease {
                        agent_index: ready.agent_index,
                        commands: Some(ready.commands),
                    });
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(AcquireError::WorkerStopped);
                }
            }
        }
    }
}

impl<B: AgentBackend> Drop for LeaseHandle<B> {
    fn drop(&mut self) {
        if let Some(cancellation) = self.cancellation.take() {
            cancellation.cancel();
        }
    }
}

/// Exclusive access to one worker and its session across any number of calls.
/// The worker cannot take another task until this lease is finished or dropped.
pub struct SessionLease<B: AgentBackend> {
    agent_index: usize,
    commands: Option<mpsc::Sender<LeaseCommand<B>>>,
}

impl<B: AgentBackend> SessionLease<B> {
    pub fn agent_index(&self) -> usize {
        self.agent_index
    }

    /// Run one request on the reserved session. The caller can validate the
    /// response outside the pool and call `ask` again before releasing it.
    pub fn ask(&mut self, request: B::Request) -> Result<B::Response, TaskError<B::Error>> {
        self.ask_with_cancellation(request, CancellationToken::default())
    }

    /// Like `ask`, with a token that another thread may cancel while the
    /// adapter is running. A non-recoverable adapter failure closes the session;
    /// a later call on this lease opens a new session on the same worker.
    pub fn ask_with_cancellation(
        &mut self,
        request: B::Request,
        cancellation: CancellationToken,
    ) -> Result<B::Response, TaskError<B::Error>> {
        let (sender, result) = mpsc::channel();
        let commands = self.commands.as_ref().ok_or(TaskError::WorkerStopped)?;
        commands
            .send(LeaseCommand::Ask {
                request,
                cancellation,
                result: sender,
            })
            .map_err(|_| TaskError::WorkerStopped)?;
        result.recv().unwrap_or(Err(TaskError::WorkerStopped))
    }

    /// Release the worker and wait until it becomes available to the queue.
    pub fn finish(mut self) -> Result<(), AcquireError<B::Error>> {
        let commands = self.commands.take().ok_or(AcquireError::WorkerStopped)?;
        let (sender, completed) = mpsc::channel();
        commands
            .send(LeaseCommand::Release {
                completed: Some(sender),
            })
            .map_err(|_| AcquireError::WorkerStopped)?;
        completed.recv().map_err(|_| AcquireError::WorkerStopped)
    }
}

impl<B: AgentBackend> Drop for SessionLease<B> {
    fn drop(&mut self) {
        if let Some(commands) = self.commands.take() {
            let _ = commands.send(LeaseCommand::Release { completed: None });
        }
    }
}
