use crate::{AgentBackend, CancellationToken, SubmitError, TaskError};
use std::sync::mpsc;

/// Result of submitting a request to a particular backend pool.
pub type SubmitResult<B> = Result<
    TaskHandle<<B as AgentBackend>::Response, <B as AgentBackend>::Error>,
    SubmitError<<B as AgentBackend>::Request>,
>;

/// One submitted task. Waiting on handles in submission order restores input
/// order even when agents complete in a different order.
pub struct TaskHandle<Response, Error> {
    pub(crate) id: u64,
    pub(crate) cancellation: CancellationToken,
    pub(crate) result: mpsc::Receiver<Result<Response, TaskError<Error>>>,
}

impl<Response, Error> TaskHandle<Response, Error> {
    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn cancel(&self) {
        self.cancellation.cancel();
    }

    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    pub fn wait(self) -> Result<Response, TaskError<Error>> {
        self.result.recv().unwrap_or(Err(TaskError::WorkerStopped))
    }
}

/// Wait for every task and return one result per input handle, in input order.
pub fn collect_ordered<Response, Error>(
    handles: impl IntoIterator<Item = TaskHandle<Response, Error>>,
) -> Vec<Result<Response, TaskError<Error>>> {
    handles.into_iter().map(TaskHandle::wait).collect()
}
