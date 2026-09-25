'use strict'

const path = require('node:path')

function nativeSuffix() {
  if (process.platform === 'win32') return `win32-${process.arch}-msvc`
  if (process.platform === 'darwin') return `darwin-${process.arch}`
  if (process.platform === 'linux') {
    const libc = process.report?.getReport()?.header?.glibcVersionRuntime ? 'gnu' : 'musl'
    return `linux-${process.arch}-${libc}`
  }
  throw new Error(`agentpools has no native build for ${process.platform}-${process.arch}`)
}

function loadNative() {
  const suffix = nativeSuffix()
  const binaries = [
    path.join(__dirname, `agentpools.${suffix}.node`),
    path.join(__dirname, 'agentpools.node'),
  ]
  for (const binary of binaries) {
    if (require('node:fs').existsSync(binary)) return require(binary)
  }
  try {
    return require(`agentpools-${suffix}`)
  } catch (error) {
    if (!error || error.code !== 'MODULE_NOT_FOUND') throw error
  }
  throw new Error(
    `agentpools native addon is missing for ${process.platform}-${process.arch}; ` +
      'build the matching target with `npm run build` or install its platform package',
  )
}

const native = loadNative()

function encodePrompt(prompt) {
  if (typeof prompt === 'string') {
    return JSON.stringify({ content: [{ type: 'text', text: prompt }] })
  }
  if (!prompt || !Array.isArray(prompt.content)) {
    throw new TypeError('prompt must be a string or an ACP prompt with a content array')
  }
  return JSON.stringify(prompt)
}

class Task {
  constructor(nativeTask) {
    this._native = nativeTask
  }

  get id() {
    return this._native.id
  }

  cancel() {
    this._native.cancel()
  }

  async result() {
    return JSON.parse(await this._native.result())
  }
}

class SessionLease {
  constructor(nativeLease, agentIndex) {
    this._native = nativeLease
    this.agentIndex = agentIndex
    this._tail = Promise.resolve()
    this._finishing = false
    this._finishPromise = null
  }

  async ask(prompt) {
    if (this._finishing) throw new Error('session lease is finished')
    const encoded = encodePrompt(prompt)
    const call = this._tail.then(() => this._native.ask(encoded))
    this._tail = call.then(() => undefined, () => undefined)
    return JSON.parse(await call)
  }

  finish() {
    if (this._finishPromise) return this._finishPromise
    this._finishing = true
    this._finishPromise = this._tail.then(() => this._native.finish()).then(() => undefined)
    return this._finishPromise
  }
}

class AgentPool {
  constructor(options) {
    if (!options || typeof options !== 'object') {
      throw new TypeError('pool options must be an object')
    }
    this._native = native.createPool(JSON.stringify(options))
  }

  submit(prompt, agentIndex) {
    return new Task(this._native.submit(encodePrompt(prompt), agentIndex))
  }

  async acquire(agentIndex) {
    const nativeLease = this._native.requestLease(agentIndex)
    try {
      const selectedAgent = await nativeLease.ready()
      return new SessionLease(nativeLease, selectedAgent)
    } catch (error) {
      nativeLease.cancel()
      throw error
    }
  }

  submitRetrying(prompt, { maxAttempts, feedbackTemplate, agentIndex } = {}) {
    if (!Number.isInteger(maxAttempts) || maxAttempts < 1) {
      throw new TypeError('maxAttempts must be a positive integer')
    }
    if (typeof feedbackTemplate !== 'string' || feedbackTemplate.length === 0) {
      throw new TypeError('feedbackTemplate must be a non-empty string')
    }
    return new Task(
      this._native.submitRetrying(
        encodePrompt(prompt),
        maxAttempts,
        feedbackTemplate,
        agentIndex,
      ),
    )
  }

  status() {
    return JSON.parse(this._native.statusJson())
  }

  async close({ drain = true } = {}) {
    return JSON.parse(await this._native.close(drain))
  }
}

module.exports = { AgentPool, SessionLease, Task }
