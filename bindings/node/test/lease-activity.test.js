'use strict'

// Lease-level cooperative controls added for textbook extraction:
// cancel() reaches the active ask; drainEvents() exposes turn-activity
// markers (turn_started/activity/turn_ended/cancelled) for stall detection.

const test = require('node:test')
const assert = require('node:assert/strict')
const fs = require('node:fs')
const path = require('node:path')
const { AgentPool } = require('..')

const root = path.resolve(__dirname, '../../..')
const mock = path.join(root, 'target', 'debug', process.platform === 'win32' ? 'mock_runtime.exe' : 'mock_runtime')

function codexPool(extraAgent = {}) {
  return new AgentPool({
    apiVersion: 2,
    agents: [
      {
        runtime: 'codexAppServer',
        program: mock,
        args: ['codex'],
        cwd: root,
        model: 'test-small-model',
        effort: 'low',
        mcpServers: [{ name: 'textbook', type: 'http', url: 'http://127.0.0.1:9/mcp' }],
        ...extraAgent,
      },
    ],
  })
}

test('pool accepts effort and mcpServers on codex workers', { skip: !fs.existsSync(mock) }, async () => {
  const pool = codexPool()
  try {
    const lease = await pool.acquire()
    try {
      // mock 把生效的 mcpServers 数量回显进回答：配置确实到达了运行时。
      assert.equal((await lease.ask('hello')).text, 'codex:hello:1:mcp-1')
    } finally {
      await lease.finish()
    }
  } finally {
    await pool.close()
  }
})

test('drainEvents reports turn lifecycle without blocking', { skip: !fs.existsSync(mock) }, async () => {
  const pool = codexPool()
  try {
    const lease = await pool.acquire()
    try {
      assert.deepEqual(lease.drainEvents(), [])
      const response = await lease.ask('hello')
      assert.match(response.text, /codex:hello/)
      const events = lease.drainEvents()
      const kinds = events.map((event) => event.kind)
      assert.ok(kinds.includes('turn_started'), `expected turn_started in ${JSON.stringify(kinds)}`)
      assert.ok(kinds.includes('turn_ended'), `expected turn_ended in ${JSON.stringify(kinds)}`)
      assert.ok(events.every((event) => typeof event.atMs === 'number'))
      assert.deepEqual(lease.drainEvents(), [])
    } finally {
      await lease.finish()
    }
  } finally {
    await pool.close()
  }
})

test('cancel on idle lease does not throw', { skip: !fs.existsSync(mock) }, async () => {
  const pool = codexPool()
  try {
    const lease = await pool.acquire()
    try {
      lease.cancel()
      assert.match((await lease.ask('hello')).text, /codex:hello/)
    } finally {
      await lease.finish()
    }
  } finally {
    await pool.close()
  }
})

test('cancel interrupts the active ask and the lease stays reusable', { skip: !fs.existsSync(mock) }, async () => {
  const pool = codexPool()
  try {
    const lease = await pool.acquire()
    try {
      const started = Date.now()
      const pending = lease.ask('wait:hello')
      await new Promise((resolve) => setTimeout(resolve, 300))
      lease.cancel()
      await assert.rejects(pending, /[Cc]ancel/)
      // 中断必须远早于 mock 的 3s 延迟返回，而不是等到底层结束。
      assert.ok(Date.now() - started < 2500, 'cancel did not interrupt the active ask promptly')
      const events = lease.drainEvents().map((event) => event.kind)
      assert.ok(events.includes('cancelled'), `expected cancelled marker, got ${JSON.stringify(events)}`)
      // 同一租约可继续使用：连续会话不断。
      assert.match((await lease.ask('again')).text, /codex:again/)
    } finally {
      await lease.finish()
    }
  } finally {
    await pool.close()
  }
})
