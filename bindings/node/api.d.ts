export interface AcpTimeouts {
  handshakeMs?: number
  promptMs?: number
  closeMs?: number
}

export interface AcpAgentOptions {
  program: string
  args?: string[]
  env?: Record<string, string>
  cwd: string
  mcpServers?: unknown[]
  authMethod?: string | null
  model?: string | null
  timeouts?: AcpTimeouts
  inheritStderr?: boolean
}

export interface AcpPoolOptions {
  apiVersion: 1
  agents: AcpAgentOptions[]
  maxQueued?: number
}

export interface AgentPrompt {
  content: unknown[]
}

export interface AcpPrompt extends AgentPrompt {}

export interface AgentResponse {
  text: string
  stopReason: string
}

export interface AcpResponse extends AgentResponse {}

export interface PoolStatus {
  queued: number
  active: number
  closed: boolean
}

export interface ShutdownReport {
  closed: true
  closeErrors: string[]
  panickedWorkers: number
}

export class SessionLease {
  readonly agentIndex: number
  ask(prompt: string | AgentPrompt): Promise<AgentResponse>
  finish(): Promise<void>
}

export class AgentPool {
  constructor(options: AcpPoolOptions | MultiRuntimePoolOptions)
  acquire(agentIndex?: number): Promise<SessionLease>
  status(): PoolStatus
  close(options?: { drain?: boolean }): Promise<ShutdownReport>
}

export interface NativeTimeouts {
  handshakeMs?: number
  promptMs?: number
}

export interface CodexAppServerAgentOptions {
  runtime: 'codexAppServer'
  program: string
  args?: string[]
  env?: Record<string, string>
  cwd: string
  model?: string | null
  timeouts?: NativeTimeouts
  inheritStderr?: boolean
}

export interface PiRpcAgentOptions {
  runtime: 'piRpc'
  program: string
  args?: string[]
  env?: Record<string, string>
  cwd: string
  timeouts?: NativeTimeouts
  inheritStderr?: boolean
}

export type RuntimeAgentOptions =
  | ({ runtime: 'acp' } & AcpAgentOptions)
  | CodexAppServerAgentOptions
  | PiRpcAgentOptions

export interface MultiRuntimePoolOptions {
  apiVersion: 2
  agents: RuntimeAgentOptions[]
  maxQueued?: number
}