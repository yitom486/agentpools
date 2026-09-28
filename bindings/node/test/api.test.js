'use strict'

const test = require('node:test')
const assert = require('node:assert/strict')

const { AgentPool } = require('..')

test('rejects invalid options before starting agent processes', () => {
  assert.throws(() => new AgentPool({ apiVersion: 2, agents: [] }))
})

test('validates lease prompts before starting an agent', async (context) => {
  const pool = new AgentPool({
    apiVersion: 1,
    agents: [{ program: 'not-started-until-acquire', cwd: process.cwd() }],
  })
  context.after(() => pool.close())
  assert.equal(pool.status().active, 0)
})
