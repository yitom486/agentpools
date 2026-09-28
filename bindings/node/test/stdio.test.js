'use strict'

const test = require('node:test')
const assert = require('node:assert/strict')
const fs = require('node:fs')
const os = require('node:os')
const path = require('node:path')

const { AgentPool } = require('..')

const workspace = path.resolve(__dirname, '../../..')
const mockAgent = path.join(
  workspace,
  'target',
  'debug',
  `agentpools-acp-mock-agent${process.platform === 'win32' ? '.exe' : ''}`,
)

function fixture(scenario = '') {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'agentpools-node-'))
  const logPath = path.join(directory, 'acp.jsonl')
  const options = {
    apiVersion: 1,
    maxQueued: 8,
    agents: [
      {
        program: mockAgent,
        cwd: workspace,
        env: {
          AGENTPOOLS_MOCK_LOG: logPath,
          AGENTPOOLS_MOCK_SCENARIO: scenario,
        },
        mcpServers: [
          { name: 'caller-calculator', command: 'calculator-mcp', args: ['--readonly'] },
        ],
      },
    ],
  }
  return { directory, logPath, options }
}

test(
  'Promise API reuses one ACP session and forwards MCP configuration',
  { skip: !fs.existsSync(mockAgent) },
  async () => {
    const testCase = fixture()
    const pool = new AgentPool(testCase.options)
    try {
      const lease = await pool.acquire()
      assert.deepEqual(await lease.ask('first task'), {
        text: 'mock:first task', stopReason: 'end_turn',
      })
      assert.deepEqual(await lease.ask('second task'), {
        text: 'mock:second task', stopReason: 'end_turn',
      })
      await lease.finish()
      const report = await pool.close({ drain: true })
      assert.deepEqual(report, { closed: true, closeErrors: [], panickedWorkers: 0 })
      assert.deepEqual(pool.status(), { queued: 0, active: 0, closed: true })

      const messages = fs
        .readFileSync(testCase.logPath, 'utf8')
        .trim()
        .split(/\r?\n/)
        .map((line) => JSON.parse(line))
      const methods = messages.map((message) => message.method).filter(Boolean)
      assert.equal(methods.filter((method) => method === 'initialize').length, 1)
      assert.equal(methods.filter((method) => method === 'session/new').length, 1)
      assert.equal(methods.filter((method) => method === 'session/prompt').length, 2)
      assert.equal(methods.filter((method) => method === 'session/close').length, 1)
      assert.deepEqual(
        messages.find((message) => message.method === 'session/new').params.mcpServers,
        testCase.options.agents[0].mcpServers,
      )
    } finally {
      await pool.close({ drain: false })
      fs.rmSync(testCase.directory, { recursive: true, force: true })
    }
  },
)

test(
  'retry feedback stays on the same worker session',
  { skip: !fs.existsSync(mockAgent) },
  async () => {
    const testCase = fixture('recover-on-feedback')
    const pool = new AgentPool(testCase.options)
    try {
      const lease = await pool.acquire()
      await assert.rejects(() => lease.ask('initial'), /mock failure/)
      const response = await lease.ask('Attempt 1 failed: mock failure. Please retry.')
      assert.match(response.text, /mock failure/)
      await lease.finish()
      await pool.close({ drain: true })

      const messages = fs
        .readFileSync(testCase.logPath, 'utf8')
        .trim()
        .split(/\r?\n/)
        .map((line) => JSON.parse(line))
      const methods = messages.map((message) => message.method).filter(Boolean)
      assert.equal(methods.filter((method) => method === 'initialize').length, 1)
      assert.equal(methods.filter((method) => method === 'session/new').length, 1)
      assert.equal(methods.filter((method) => method === 'session/prompt').length, 2)
      assert.equal(methods.filter((method) => method === 'session/close').length, 1)
    } finally {
      await pool.close({ drain: false })
      fs.rmSync(testCase.directory, { recursive: true, force: true })
    }
  },
)

test(
  'external validation keeps a session leased until the caller finishes',
  { skip: !fs.existsSync(mockAgent) },
  async () => {
    const testCase = fixture()
    const pool = new AgentPool(testCase.options)
    let lease
    try {
      lease = await pool.acquire()
      assert.equal(lease.agentIndex, 0)
      assert.deepEqual(await lease.ask('draft'), {
        text: 'mock:draft',
        stopReason: 'end_turn',
      })
      const queued = pool.acquire()
      // The caller validates the draft here, outside any agent call.
      assert.deepEqual(pool.status(), { queued: 1, active: 1, closed: false })
      assert.deepEqual(await lease.ask('correction'), {
        text: 'mock:correction',
        stopReason: 'end_turn',
      })
      assert.equal(pool.status().queued, 1)
      await lease.finish()
      lease = null
      const nextLease = await queued
      assert.deepEqual(await nextLease.ask('next task'), {
        text: 'mock:next task',
        stopReason: 'end_turn',
      })
      await nextLease.finish()
      await pool.close({ drain: true })

      const messages = fs.readFileSync(testCase.logPath, 'utf8').trim().split(/\r?\n/).map(JSON.parse)
      assert.equal(messages.filter((message) => message.method === 'session/new').length, 1)
      assert.deepEqual(
        messages.filter((message) => message.method === 'session/prompt')
          .map((message) => message.params.prompt[0].text),
        ['draft', 'correction', 'next task'],
      )
    } finally {
      if (lease) await lease.finish()
      await pool.close({ drain: false })
      fs.rmSync(testCase.directory, { recursive: true, force: true })
    }
  },
)
