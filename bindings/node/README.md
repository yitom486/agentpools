# agentpools for Node.js

Promise-based Node.js API backed by the shared Rust scheduler and ACP stdio
adapter. Each configured `agents` entry owns one persistent worker/session
slot. Caller-provided `mcpServers` are passed through to ACP `session/new`.

## Local development

Install Node.js 20 or newer, Rust, and the platform linker, then run:

```sh
npm install
npm run build
npm test
```

## Example

```js
const { AgentPool } = require('agentpools')

const pool = new AgentPool({
  apiVersion: 1,
  maxQueued: 32,
  agents: [
    { program: 'codex-acp', args: [], cwd: process.cwd(), model: 'gpt-6-luna' },
  ],
})

async function main() {
  try {
    const task = pool.submit('Add 2 and 3.')
    console.log(await task.result())
  } finally {
    await pool.close({ drain: true })
  }
}

main().catch(console.error)
```

`submitRetrying` requires an explicit feedback template. `{error}` and
`{attempt}` are substituted and prepended to the next prompt. Retry happens
only when the ACP adapter confirms the existing session can safely continue.

For validation in application code after an answer, reserve a worker with a
session lease. The queued work cannot use that worker until `finish()`:

```js
const lease = await pool.acquire()
try {
  let answer = await lease.ask('Create a draft')
  while (needsCorrection(answer)) {
    answer = await lease.ask(correctionFor(answer))
  }
} finally {
  await lease.finish()
}
```

`pool.acquire(agentIndex)` can reserve a specific worker. Release all leases
before calling `pool.close()`, which waits for active workers.

The workspace [language API contract](../../docs/language-api.md) describes
configuration, task, cancellation, retry, and shutdown semantics. The npm
package still needs its full platform build matrix and artifact collection
test before publication. Build each configured target in CI, upload the
platform-suffixed `.node` files into `artifacts/`, then run
`npm run release:collect` and `npm run release:prepare`. These steps populate
the per-platform packages and the root package's optional dependencies. Review
the staged manifests before publishing the platform packages and then the root
package. The checked-in platform manifests are generated under `npm/` by
`napi create-npm-dirs`.
