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
      const first = pool.submit('first task')
      const second = pool.submit('second task')
      assert.equal(typeof first.id, 'string')
      assert.deepEqual(await Promise.all([first.result(), second.result()]), [
        { text: 'mock:first task', stopReason: 'end_turn' },
        { text: 'mock:second task', stopReason: 'end_turn' },
      ])
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
      const task = pool.submitRetrying('initial', {
        maxAttempts: 2,
        feedbackTemplate: 'Attempt {attempt} failed: {error}. Please retry.',
      })
      const response = await task.result()
      assert.match(response.text, /mock failure/)
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
