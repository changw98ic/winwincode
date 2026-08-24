/** Durable, provider-neutral facts projected from the embedded Codex runtime. */

export const RUNTIME_EVENT_SCHEMA_VERSION = 1 as const

export const RUNTIME_EVENT_KINDS = Object.freeze([
  'session.configured',
  'turn.started',
  'turn.completed',
  'turn.aborted',
  'item.started',
  'item.completed',
  'plan.updated',
  'message.delta',
  'message.completed',
  'reasoning.delta',
  'reasoning.completed',
  'tool.started',
  'tool.output',
  'tool.completed',
  'approval.requested',
  'input.requested',
  'diff.updated',
  'usage.updated',
  'subagent.started',
  'subagent.updated',
  'subagent.completed',
  'failure',
  'warning',
  'notice',
] as const)

export type RuntimeEventKind = typeof RUNTIME_EVENT_KINDS[number]

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

export type RuntimePlanItemStatus = 'pending' | 'in_progress' | 'completed'

export interface RuntimePlanItem {
  readonly step: string
  readonly status: RuntimePlanItemStatus
}

export interface RuntimePlanSemantic {
  readonly kind: 'plan'
  readonly mode: 'started' | 'delta' | 'snapshot' | 'completed'
  readonly itemId: string | null
  readonly explanation: string | null
  readonly items: readonly RuntimePlanItem[]
  readonly text: string | null
}

export interface RuntimeInputQuestionOption {
  readonly label: string
  readonly description: string
}

export interface RuntimeInputQuestion {
  readonly id: string
  readonly header: string
  readonly question: string
  readonly isOther: boolean
  readonly isSecret: boolean
  readonly options: readonly RuntimeInputQuestionOption[]
}

export interface RuntimeInputSemantic {
  readonly kind: 'input'
  readonly requestId: string
  readonly blocking: boolean
  readonly questions: readonly RuntimeInputQuestion[]
}

export interface RuntimeAgentGraphChange {
  readonly threadId: string
  readonly path: string | null
  readonly parentThreadId: string | null
  readonly nickname: string | null
  readonly role: string | null
  readonly status: string
  readonly action:
    | 'started'
    | 'updated'
    | 'waiting'
    | 'completed'
    | 'interrupted'
    | 'closed'
    | 'resumed'
}

export interface RuntimeAgentGraphSemantic {
  readonly kind: 'agent-graph'
  readonly changes: readonly RuntimeAgentGraphChange[]
}

export type RuntimeVerificationEvidenceType =
  | 'test'
  | 'command'
  | 'diff'
  | 'file'
  | 'commit'
  | 'runtime_event'

export interface RuntimeVerificationEvidenceSource {
  readonly type: RuntimeVerificationEvidenceType
  readonly eventId: string
}

export const RUNTIME_VERIFICATION_RESULT_PROTOCOL =
  'winwincode.independent-verification-result.v1' as const

/** One finding inside a validated Codex reviewer or verifier final response. */
export interface RuntimeVerificationFinding {
  readonly findingId: string
  readonly criterionId: string | null
  readonly verdict: 'pass' | 'fail' | 'inconclusive' | 'infra_error'
  readonly explanation: string
  readonly evidenceSources: readonly RuntimeVerificationEvidenceSource[]
}

/** Strict JSON final response projected from an independent Codex session. */
export interface RuntimeVerificationResultSemantic {
  readonly kind: 'verification-result'
  readonly protocol: typeof RUNTIME_VERIFICATION_RESULT_PROTOCOL
  readonly deliverySpecId: string
  readonly deliverySpecRevision: number
  readonly candidateRef: string
  readonly findings: readonly RuntimeVerificationFinding[]
}

export type RuntimeSemanticPayload =
  | RuntimePlanSemantic
  | RuntimeInputSemantic
  | RuntimeAgentGraphSemantic
  | RuntimeVerificationResultSemantic

/** One normalized fact. It remains a projection of Codex, never an execution command. */
export interface RuntimeEvent {
  readonly schemaVersion: typeof RUNTIME_EVENT_SCHEMA_VERSION
  readonly id: string
  readonly cursor: RuntimeCursor
  readonly kind: RuntimeEventKind
  readonly source: RuntimeSourceIdentity
  readonly occurredAtMillis?: number
  readonly terminalReason?: RuntimeTerminalReason
  readonly semantic?: RuntimeSemanticPayload
  readonly data: Readonly<Record<string, unknown>>
}

/** Deterministic identity used for replay deduplication and downstream effects. */
export function runtimeEventId(sessionId: string, sequence: string): string {
  return `${sessionId}@${sequence}`
}
