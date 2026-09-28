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
