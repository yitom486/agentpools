//! A bounded, reusable session pool for agent-backed tasks.
//!
//! The pool schedules requests and owns session lifetimes. An [`AgentBackend`]
//! owns the protocol, tools, permissions, and the exact session configuration.
//! In particular, an ACP adapter can forward caller-provided MCP servers
//! unchanged when it opens a session; this crate does not parse MCP settings.

mod backend;
mod cancellation;
mod error;
mod lease;
mod pool;

pub use backend::{AgentBackend, AgentSession};
pub use cancellation::CancellationToken;
pub use error::{AcquireError, BuildError, TaskError};
pub use lease::{LeaseHandle, SessionLease};
pub use pool::{AgentPool, PoolConfig, PoolStatus, ShutdownMode, ShutdownReport};
