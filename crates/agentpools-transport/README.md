# agentpools-transport

`agentpools-transport` provides cross-platform stdio child process management, line buffering, and JSON stream framing for `agentpools` adapters.

## Core Capabilities

- **Cross-Platform Process Lifecycle (`terminate_child`)**:
  - Handles graceful exit by closing `stdin` (triggering EOF) and waiting with a configurable grace period.
  - Falls back to `child.kill()`, cleanly handling Windows-specific errors (`ErrorKind::InvalidInput`, `ErrorKind::PermissionDenied`) when a process has already terminated.
  - Joins background reader threads cleanly to prevent zombie threads and handle leaks on Linux, macOS, and Windows.
- **Cross-Platform Line & JSON Streaming (`JsonProcess`)**:
  - Handles line endings seamlessly across platforms (`\n` and `\r\n`), stripping carriage returns and skipping blank lines.
  - Background reader thread parses incoming lines into `serde_json::Value` without blocking the main event loops.
  - Thread-safe `send()` with automatic serialization and newline flushing.
  - Built-in out-of-order response matching (`response()`) with bounded pending queue size to prevent memory leaks during long-running streaming interactions.
