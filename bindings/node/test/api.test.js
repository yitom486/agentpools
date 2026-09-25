'use strict'

const test = require('node:test')
const assert = require('node:assert/strict')

const { AgentPool } = require('..')

test('rejects invalid options before starting agent processes', () => {
  assert.throws(() => new AgentPool({ apiVersion: 2, agents: [] }))
})

test('formats task prompts and validates retry options', async (context) => {
  const pool = new AgentPool({
    apiVersion: 1,
    agents: [{ program: 'not-started-until-submit', cwd: process.cwd() }],
  })
  context.after(() => pool.close())
  assert.throws(
    () =>
      pool.submitRetrying('hello', { maxAttempts: 0, feedbackTemplate: 'retry {error}' }),
    /maxAttempts/,
  )
})
