# Language binding API contract

This document defines the shared configuration and task value shapes for the
Node.js and Python bindings. Both bindings call the same Rust
`agentpools` scheduler and `agentpools-runtime` adapter; they will not reimplement
scheduling in JavaScript or Python.

## Scope and versioning

API v1 is an ACP stdio process pool. Each item in `agents` owns one worker slot
and one reusable ACP process/session. The config has an explicit `apiVersion`
so bindings can reject unsupported contracts before spawning agents. Unknown
fields are rejected to catch misspelled options.

This section describes the compatible ACP API v1. The bindings call
`agentpools-runtime::build_pool`, which also accepts tagged API v2 configs for ACP,
Codex app-server, and Pi RPC. See the [multi-runtime usage guide](multi-runtime-usage.md).
The Rust ACP adapter also has
an opt-in `SharedAcpBackend` / `AcpPoolOptions::build_shared()` path, where
multiple worker sessions share one ACP process. The bindings do not expose
that path or a JSON switch for it in API v1. See the
[execution-mode guide](execution-modes-and-reports.md).

The bindings expose the scheduler lifecycle as native async APIs:

- construct a pool from ACP API v1 or tagged runtime API v2 options;
- acquire any worker or target an agent index;
- call lease.ask(prompt), validate, and repeat while retaining that worker;
- call lease.finish() or exit Python's async context to release it;
- inspect status and close the pool.

The worker remains reserved while application code validates the response. A recoverable ACP error may be sent back as feedback in a later ask on the same session. An uncertain error closes the session; the lease keeps the worker and a later ask opens a fresh session. Node.js exposes the worker index as agentIndex, Python as agent_index.

Session setup is lazy: creating a pool starts worker threads, and acquiring a worker opens its ACP session. Release all leases before a draining shutdown.

## Configuration shape

```json
{
  "apiVersion": 1,
  "maxQueued": 128,
  "agents": [
    {
      "program": "codex-acp",
      "args": ["--stdio"],
      "env": {"CODEX_MODEL": "gpt-6-luna"},
      "cwd": "C:/work/project",
      "mcpServers": [
        {"name": "calculator", "command": "calculator-mcp", "args": []}
      ],
      "authMethod": null,
      "model": "gpt-6-luna",
      "timeouts": {
        "handshakeMs": 30000,
        "promptMs": 300000,
        "closeMs": 3000
      },
      "inheritStderr": false
    }
  ]
}
```

`maxQueued` defaults to 128. Timeout values use milliseconds and default to
30,000 / 300,000 / 3,000 respectively. Every worker `cwd` must be absolute;
commands and all three timeout values must be valid before a pool is created.
Each configured agent corresponds to exactly one worker and ACP session slot.

The `mcpServers` array is opaque JSON. The adapter forwards its values to ACP
`session/new`; the scheduler and bindings do not inspect, merge, or rewrite
them. MCP server process launch and tool permissions remain the ACP agent's
responsibility.

API v1 does not advertise ACP client capabilities. In particular, permission,
filesystem, and terminal callbacks are not silently enabled. An ACP permission
request without a host handler is answered as cancelled by the adapter. A
future binding API can add explicit host callbacks without changing the
versioned pool configuration.

## Task values

An ACP prompt is an object with a `content` array of ACP content blocks. For a
plain text task:

```json
{"content": [{"type": "text", "text": "add 2 and 3"}]}
```

The response has `text` and `stopReason` fields. The adapter currently
assembles text updates into `text`; it does not expose raw ACP update events.
Bindings should preserve that shape and report construction/task errors as
language-native exceptions with the original error message.

## Scheduler semantics bindings must preserve

- A SessionLease owns one worker across all calls, validation, and feedback until finish or drop. Other workers continue independently.
- Retry is a caller decision. The adapter allows original-session reuse only when the protocol remains synchronized.
- An uncertain error closes the session; the lease stays on its worker, and the next ask opens a new session.
- Queued leases are bounded and cancellable. Shutdown waits for active leases.
- MCP configuration belongs to a session; use distinct agent entries or pools for distinct tool sets.

The Rust source of these serialized types is `agentpools_acp::{AcpPoolOptions,
AcpAgentOptions, AcpTimeoutOptions, AcpPrompt, AcpResponse}`. JSON serialization
tests in the crate guard the camelCase field names and MCP pass-through shape.
