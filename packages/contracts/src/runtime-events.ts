/** Durable, provider-neutral facts projected from the embedded Codex runtime. */

export const RUNTIME_EVENT_SCHEMA_VERSION = 1 as const

export type RuntimeEventKind =
  | 'session.configured'
  | 'turn.started'
  | 'turn.completed'
  | 'turn.aborted'
  | 'item.started'
  | 'item.completed'
  | 'message.delta'
  | 'message.completed'
  | 'reasoning.delta'
  | 'reasoning.completed'
  | 'tool.started'
  | 'tool.output'
  | 'tool.completed'
  | 'approval.requested'
  | 'diff.updated'
  | 'usage.updated'
  | 'subagent.started'
  | 'subagent.updated'
  | 'subagent.completed'
  | 'failure'
  | 'warning'
  | 'notice'

export type RuntimeTerminalReason =
  | 'completed'
  | 'aborted'
  | 'failed'
  | 'declined'
  | 'cancelled'
  | 'unknown'

/** Stable source identity retained by every projection and UI row. */
export interface RuntimeSourceIdentity {
  readonly authority: 'codex-core'
  /** DSH/product session identity used by projections and user-facing state. */
  readonly sessionId: string
  /** Native Codex session identity used for control calls and event delivery. */
  readonly kernelSessionId: string
  readonly roleId: string
  /** Stable identity of one native event-pump lifetime. */
  readonly kernelStreamId: string
  readonly kernelSequence: string
  readonly submissionId: string
  readonly kernelKind: string
  readonly turnId?: string
  readonly itemId?: string
  readonly toolCallId?: string
  readonly approvalId?: string
  readonly agentThreadId?: string
  readonly agentPath?: string
}

/** Monotonic cursor within one kernel session. */
export interface RuntimeCursor {
  readonly sessionId: string
  readonly sequence: string
}

/** One normalized fact. It remains a projection of Codex, never an execution command. */
export interface RuntimeEvent {
  readonly schemaVersion: typeof RUNTIME_EVENT_SCHEMA_VERSION
  readonly id: string
  readonly cursor: RuntimeCursor
  readonly kind: RuntimeEventKind
  readonly source: RuntimeSourceIdentity
  readonly occurredAtMillis?: number
  readonly terminalReason?: RuntimeTerminalReason
  readonly data: Readonly<Record<string, unknown>>
}

/** Deterministic identity used for replay deduplication and downstream effects. */
export function runtimeEventId(sessionId: string, sequence: string): string {
  return `${sessionId}@${sequence}`
}
