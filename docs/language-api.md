# Language binding API contract

This document defines the shared configuration and task value shapes for the
Node.js and Python bindings. Both bindings call the same Rust
`agentpools` scheduler and `agentpools-acp` adapter; they will not reimplement
scheduling in JavaScript or Python.

## Scope and versioning

API v1 is an ACP stdio process pool. Each item in `agents` owns one worker slot
and one reusable ACP process/session. The config has an explicit `apiVersion`
so bindings can reject unsupported contracts before spawning agents. Unknown
fields are rejected to catch misspelled options.

The bindings expose the scheduler lifecycle as native async APIs:

- construct a pool from `AcpPoolOptions`;
- submit an ACP prompt to any available agent, or target an agent index;
- await the response and cancel a task;
- inspect aggregate queue/active/closed status;
- drain or cancel queued work, then close the pool.

An explicit `submitRetrying` / `submit_retrying` call also accepts a
`feedbackTemplate`; `{error}` and `{attempt}` are replaced before the text is
prepended as the next ACP prompt block. `maxAttempts` / `max_attempts` counts
the initial call as attempt one. Retry remains adapter-gated: only a
recoverable error on a synchronized session reaches the same worker/session.

ACP session setup is lazy: constructing the pool validates configuration and
starts worker threads; an ACP process starts when its worker receives its first
task. Node promises and Python `asyncio` calls must not block their event loop
while waiting for Rust task handles.

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

- One worker owns one session at a time; a submitted task and its retries keep
  that worker until the task succeeds, is abandoned, or is cancelled.
- Retry is opt-in. Only errors for which the adapter reports the session is
  still synchronized may be retried on that same session.
- A normal failed prompt discards the uncertain session. The next task on that
  worker opens a fresh ACP process/session.
- MCP config is session-scoped. Use separate agent entries or pools when tool
  sets differ; task submission does not infer tool compatibility.
- Shutdown joins workers and closes sessions. ACP I/O timeouts bound that
  operation; dropping a pool cancels queued work and waits for running calls.

The Rust source of these serialized types is `agentpools_acp::{AcpPoolOptions,
AcpAgentOptions, AcpTimeoutOptions, AcpPrompt, AcpResponse}`. JSON serialization
tests in the crate guard the camelCase field names and MCP pass-through shape.
