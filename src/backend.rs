use crate::CancellationToken;

/// The protocol-specific session owned by one worker at a time.
///
/// `run` may contain several prompt or tool interactions. Implementations should use
/// finite I/O timeouts and cooperate with `cancellation` when possible.
pub trait AgentSession<Request, Response, Error>: Send {
    fn run(
        &mut self,
        request: Request,
        cancellation: &CancellationToken,
    ) -> Result<Response, Error>;

    fn close(&mut self) -> Result<(), Error>;

    /// Whether a failed `run` left the same session ready for another request.
    /// Returning `false` is the safe default for broken transports and unknown
    /// protocol state. The pool only retries on this session when it is `true`.
    fn can_retry_after(&self, _error: &Error) -> bool {
        false
    }
}

/// Creates sessions for the pool. `Config` belongs entirely to the adapter.
///
/// The pool stores one immutable config and passes it by reference to every
/// `open` call. An adapter can therefore use a typed config containing MCP
/// servers, tool permissions, credentials, model settings, or other options.
pub trait AgentBackend: Send + Sync + 'static {
    type Config: Send + Sync + 'static;
    type Request: Send + 'static;
    type Response: Send + 'static;
    type Error: Send + 'static;
    type Session: AgentSession<Self::Request, Self::Response, Self::Error> + 'static;

    fn open(&self, config: &Self::Config) -> Result<Self::Session, Self::Error>;
}
