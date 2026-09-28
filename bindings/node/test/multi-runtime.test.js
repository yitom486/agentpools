'use strict'

const test = require('node:test')
const assert = require('node:assert/strict')
const fs = require('node:fs')
const path = require('node:path')
const { AgentPool } = require('..')

const root = path.resolve(__dirname, '../../..')
const mock = path.join(root, 'target', 'debug', process.platform === 'win32' ? 'mock_runtime.exe' : 'mock_runtime')

test('API v2 uses Codex and Pi workers in the same pool', { skip: !fs.existsSync(mock) }, async () => {
  const pool = new AgentPool({
    apiVersion: 2,
    agents: [
      { runtime: 'codexAppServer', program: mock, args: ['codex'], cwd: root },
      { runtime: 'piRpc', program: mock, args: ['pi'], cwd: root },
    ],
  })
  try {
    for (const [index, prefix] of [[0, 'codex'], [1, 'pi']]) {
      const lease = await pool.acquire(index)
      try {
        assert.equal((await lease.ask('hello')).text, `${prefix}:hello:1`)
        assert.equal((await lease.ask('again')).text, `${prefix}:again:2`)
      } finally {
        await lease.finish()
      }
    }
  } finally {
    await pool.close()
  }
})

test('API v2 supports sharedProcess multiplexing and ephemeral sessions for Codex', { skip: !fs.existsSync(mock) }, async () => {
  const pool = new AgentPool({
    apiVersion: 2,
    sharedProcess: true,
    agents: [
      { runtime: 'codexAppServer', program: mock, args: ['codex'], cwd: root, ephemeral: true },
      { runtime: 'codexAppServer', program: mock, args: ['codex'], cwd: root, ephemeral: true },
    ],
  })
  try {
    const l1 = await pool.acquire(0)
    const l2 = await pool.acquire(1)
    try {
      assert.equal((await l1.ask('alpha')).text, 'codex:alpha:1')
      assert.equal((await l2.ask('beta')).text, 'codex:beta:2')
    } finally {
      await l1.finish()
      await l2.finish()
    }
  } finally {
    await pool.close()
  }
})
