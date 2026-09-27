use std::{fmt, io};

#[derive(Debug)]
pub enum BuildError {
    InvalidConfig,
    Spawn(io::Error),
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig => write!(f, "workers and max_queued must be positive"),
            Self::Spawn(error) => write!(f, "cannot start pool worker: {error}"),
        }
    }
}

impl std::error::Error for BuildError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Spawn(error) => Some(error),
            Self::InvalidConfig => None,
        }
    }
}

/// A rejected submission retains ownership of its request.
#[derive(Debug)]
pub enum SubmitError<Request> {
    Closed(Request),
    QueueFull(Request),
    NoSuchAgent(Request),
}

impl<Request> fmt::Display for SubmitError<Request> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed(_) => write!(f, "agent pool is closed"),
            Self::QueueFull(_) => write!(f, "agent pool queue is full"),
            Self::NoSuchAgent(_) => write!(f, "agent index does not exist"),
        }
    }
}

/// A task failure. Ordinary submitted tasks close their session after a failed
/// `run`; a leased session remains usable only if its adapter confirms that
/// the failed call left the protocol synchronized.
#[derive(Debug)]
pub enum TaskError<Error> {
    Open(Error),
    Run { source: Error, close: Option<Error> },
    Cancelled,
    WorkerStopped,
}

impl<Error: fmt::Display> fmt::Display for TaskError<Error> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Open(error) => write!(f, "cannot open agent session: {error}"),
            Self::Run { source, .. } => write!(f, "agent task failed: {source}"),
            Self::Cancelled => write!(f, "agent task cancelled"),
            Self::WorkerStopped => write!(f, "agent worker stopped"),
        }
    }
}

impl<Error: std::error::Error + 'static> std::error::Error for TaskError<Error> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Open(error) | Self::Run { source: error, .. } => Some(error),
            Self::Cancelled | Self::WorkerStopped => None,
        }
    }
}

/// Failure to reserve an exclusive worker session.
#[derive(Debug)]
pub enum AcquireError<Error> {
    Closed,
    QueueFull,
    NoSuchAgent,
    Open(Error),
    Cancelled,
    WorkerStopped,
}

impl<Error: fmt::Display> fmt::Display for AcquireError<Error> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => write!(f, "agent pool is closed"),
            Self::QueueFull => write!(f, "agent pool queue is full"),
            Self::NoSuchAgent => write!(f, "agent index does not exist"),
            Self::Open(error) => write!(f, "cannot open agent session: {error}"),
            Self::Cancelled => write!(f, "session acquisition was cancelled"),
            Self::WorkerStopped => write!(f, "agent worker stopped"),
        }
    }
}

impl<Error: std::error::Error + 'static> std::error::Error for AcquireError<Error> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Open(error) => Some(error),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::BuildError;
    use std::error::Error;
    use std::io;

    #[test]
    fn spawn_error_exposes_io_error_as_source() {
        let error = BuildError::Spawn(io::Error::other("worker spawn failed"));

        let source = error.source().expect("spawn error should have a source");
        assert!(source.downcast_ref::<io::Error>().is_some());
    }

    #[test]
    fn invalid_config_has_no_source() {
        let error = BuildError::InvalidConfig;

        assert!(error.source().is_none());
    }
}