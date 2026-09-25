# agentpools for Python

Async Python wrapper around the shared Rust scheduler and ACP stdio adapter.

## Development build

Install Python 3.9 or newer and maturin, then run `maturin develop` from this
directory. The project uses PyO3's CPython stable ABI so one wheel can support
the declared Python minor-version range on each built platform.

See the workspace [`docs/language-api.md`](../../docs/language-api.md) for the
shared configuration, prompt, retry, and shutdown semantics.

Run the async integration tests with `python -m unittest discover -s test -v`
after building the ACP mock agent with
`cargo build -p agentpools-acp --features test-support --bin agentpools-acp-mock-agent`.
