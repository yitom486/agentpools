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

export interface AcpPrompt {
  content: unknown[]
}

export interface AcpResponse {
  text: string
  stopReason: string
}

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

export class Task {
  readonly id: string
  cancel(): void
  result(): Promise<AcpResponse>
}

export class SessionLease {
  readonly agentIndex: number
  ask(prompt: string | AcpPrompt): Promise<AcpResponse>
  finish(): Promise<void>
}

export class AgentPool {
  constructor(options: AcpPoolOptions)
  acquire(agentIndex?: number): Promise<SessionLease>
  submit(prompt: string | AcpPrompt, agentIndex?: number): Task
  submitRetrying(
    prompt: string | AcpPrompt,
    options: {
      maxAttempts: number
      feedbackTemplate: string
      agentIndex?: number
    },
  ): Task
  status(): PoolStatus
  close(options?: { drain?: boolean }): Promise<ShutdownReport>
}
