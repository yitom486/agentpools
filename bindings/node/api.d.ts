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
  ephemeral?: boolean
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

export interface ActivityEvent {
  kind: 'turn_started' | 'activity' | 'turn_ended' | 'cancelled'
  turnId: string | null
  atMs: number
}

export class SessionLease {
  readonly agentIndex: number
  ask(prompt: string | AgentPrompt): Promise<AgentResponse>
  /** Cooperative cancel for the active ask; safe when idle. */
  cancel(): void
  /** Drain turn-activity markers recorded since the last call; never blocks. */
  drainEvents(): ActivityEvent[]
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
  /** Reasoning effort forwarded to every Codex turn/start. Absent means server default. */
  effort?: string | null
  /** MCP servers injected via thread/start config.mcp_servers (stdio or http entries). */
  mcpServers?: unknown[]
  /** Thread approval policy. Absent means the native `"never"` default (anything needing approval is denied). */
  approvalPolicy?: string | null
  /** Opt-in auto-approval for MCP tool-call elicitations. Default false; enable only for first-party localhost tool servers. */
  autoApproveMcpToolCalls?: boolean | null
  /** Sandbox for model-executed shell commands (official SandboxMode). Absent = server default. */
  sandbox?: string | null
  timeouts?: NativeTimeouts
  inheritStderr?: boolean
  ephemeral?: boolean
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
  sharedProcess?: boolean
}
