# agentpools for Python

Async Python wrapper around the shared Rust scheduler with ACP, Codex app-server, and Pi RPC adapters.

## Development build

Install Python 3.9 or newer and maturin, then run `maturin develop` from this
directory. The project uses PyO3's CPython stable ABI so one wheel can support
the declared Python minor-version range on each built platform.

See the workspace [`docs/language-api.md`](../../docs/language-api.md) for the
shared configuration, prompt, lease, and shutdown semantics.

Reserve a worker while application code checks an answer and sends corrections:

```python
async with await pool.acquire() as lease:
    answer = await lease.ask("Create a draft")
    while needs_correction(answer):
        answer = await lease.ask(correction_for(answer))
```

`pool.acquire(agent_index)` can target a specific worker. The worker remains
reserved during local validation. Exit the lease context before closing the
pool, because shutdown waits for active workers.

Run the async integration tests with `python -m unittest discover -s test -v`
after building the ACP mock agent with
`cargo build -p agentpools-acp --features test-support --bin agentpools-acp-mock-agent`.

For API v2 Codex app-server, Pi RPC, and mixed-runtime configuration, see the [multi-runtime usage guide](../../docs/multi-runtime-usage.md).
