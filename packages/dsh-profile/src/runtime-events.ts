import {
  RUNTIME_EVENT_SCHEMA_VERSION,
  runtimeEventId,
  type RuntimeCursor,
  type RuntimeEvent,
  type RuntimeEventKind,
  type RuntimeSourceIdentity,
  type RuntimeTerminalReason,
} from '@winwincode/contracts'
import type { KernelEvent } from '@winwincode/native'

const DEFAULT_REMEMBERED_EVENT_LIMIT = 2_048
const MAX_REMEMBERED_EVENT_LIMIT = 65_536

export type RuntimeProjectionErrorCode =
  | 'INVALID_PROJECTOR_OPTIONS'
  | 'INVALID_KERNEL_EVENT'
  | 'EVENT_SEQUENCE_MISSING'
  | 'EVENT_SEQUENCE_CONFLICT'
  | 'EVENT_SEQUENCE_OUT_OF_ORDER'

/** Visible failure at the Codex-to-product projection boundary. */
export class RuntimeProjectionError extends Error {
  readonly code: RuntimeProjectionErrorCode
  readonly sessionId: string
  readonly expectedSequence?: string
  readonly actualSequence?: string

  constructor(
    code: RuntimeProjectionErrorCode,
    message: string,
    facts: {
      readonly sessionId: string
      readonly expectedSequence?: string
      readonly actualSequence?: string
    },
  ) {
    super(message)
    this.name = 'RuntimeProjectionError'
    this.code = code
    this.sessionId = facts.sessionId
    if (facts.expectedSequence !== undefined) this.expectedSequence = facts.expectedSequence
    if (facts.actualSequence !== undefined) this.actualSequence = facts.actualSequence
  }
}

export interface CodexRuntimeProjectorOptions {
  /** DSH/product session identity used by normalized cursors. */
  readonly sessionId: string
  /** Native Codex session identity. Defaults to sessionId for direct kernel consumers. */
  readonly kernelSessionId?: string
  readonly roleId: string
  /** Stable identity for this native event-pump lifetime. */
  readonly kernelStreamId: string
  /** Last normalized sequence already committed for this session. */
  readonly startAfterSequence?: bigint | number | string
  /** Last native sequence already consumed from this exact kernel stream. */
  readonly kernelStartAfterSequence?: bigint | number | string
  readonly rememberedEventLimit?: number
}

export interface CodexRuntimeCheckpoint {
  readonly cursor: RuntimeCursor
  readonly kernelStreamId: string
  readonly kernelSequence: string
}

export interface KernelEventSource {
  events(
    sessionId: string,
    options?: { readonly signal?: AbortSignal; readonly timeoutMillis?: number },
  ): AsyncIterable<KernelEvent>
}

export interface RuntimeEventStreamOptions extends CodexRuntimeProjectorOptions {
  readonly signal?: AbortSignal
  readonly timeoutMillis?: number
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

function nonEmptyString(value: unknown): string | undefined {
  return typeof value === 'string' && value.length > 0 ? value : undefined
}

function sequenceValue(value: bigint | number | string | undefined, label: string): bigint {
  if (value === undefined) return 0n
  if (typeof value === 'bigint') {
    if (value < 0n) throw new Error(`${label} must not be negative`)
    return value
  }
  if (typeof value === 'number') {
    if (!Number.isSafeInteger(value) || value < 0) {
      throw new Error(`${label} must be a non-negative safe integer`)
    }
    return BigInt(value)
  }
  if (!/^\d+$/u.test(value)) throw new Error(`${label} must be an unsigned decimal string`)
  return BigInt(value)
}

function boundedLimit(value: number | undefined): number {
  const limit = value ?? DEFAULT_REMEMBERED_EVENT_LIMIT
  if (!Number.isSafeInteger(limit) || limit < 1 || limit > MAX_REMEMBERED_EVENT_LIMIT) {
    throw new Error(
      `rememberedEventLimit must be between 1 and ${MAX_REMEMBERED_EVENT_LIMIT}`,
    )
  }
  return limit
}

function freezeRecord(value: Record<string, unknown>): Readonly<Record<string, unknown>> {
  const clone = structuredClone(value)
  const pending: object[] = [clone]
  while (pending.length > 0) {
    const current = pending.pop()
    if (current === undefined || Object.isFrozen(current)) continue
    Object.freeze(current)
    for (const child of Object.values(current)) {
      if (typeof child === 'object' && child !== null) pending.push(child)
    }
  }
  return clone
}

function nestedRecord(record: Record<string, unknown>, key: string): Record<string, unknown> | undefined {
  const value = record[key]
  return isRecord(value) ? value : undefined
}

function itemType(message: Record<string, unknown>): string | undefined {
  return nonEmptyString(nestedRecord(message, 'item')?.type)
}

function isToolItem(type: string | undefined): boolean {
  return type === 'CommandExecution'
    || type === 'DynamicToolCall'
    || type === 'CollabAgentToolCall'
    || type === 'WebSearch'
    || type === 'ImageView'
    || type === 'ImageGeneration'
    || type === 'FileChange'
    || type === 'McpToolCall'
    || type === 'Extension'
}

function normalizedKind(type: string, message: Record<string, unknown>): RuntimeEventKind {
  switch (type) {
    case 'session_configured':
    case 'thread_settings_applied':
      return 'session.configured'
    case 'task_started':
    case 'turn_started':
      return 'turn.started'
    case 'task_complete':
    case 'turn_complete':
      return 'turn.completed'
    case 'turn_aborted':
      return 'turn.aborted'
    case 'item_started': {
      const typeOfItem = itemType(message)
      if (typeOfItem === 'SubAgentActivity') return 'subagent.started'
      if (isToolItem(typeOfItem)) return 'tool.started'
      return 'item.started'
    }
    case 'item_completed': {
      const typeOfItem = itemType(message)
      if (typeOfItem === 'AgentMessage' || typeOfItem === 'UserMessage') {
        return 'message.completed'
      }
      if (typeOfItem === 'Reasoning') return 'reasoning.completed'
      if (typeOfItem === 'SubAgentActivity') return 'subagent.completed'
      if (isToolItem(typeOfItem)) return 'tool.completed'
      return 'item.completed'
    }
    case 'user_message':
    case 'agent_message':
      return 'message.completed'
    case 'agent_message_content_delta':
    case 'plan_delta':
      return 'message.delta'
    case 'agent_reasoning':
    case 'agent_reasoning_raw_content':
      return 'reasoning.completed'
    case 'reasoning_content_delta':
    case 'reasoning_raw_content_delta':
    case 'agent_reasoning_section_break':
      return 'reasoning.delta'
    case 'exec_command_begin':
    case 'mcp_tool_call_begin':
    case 'web_search_begin':
    case 'image_generation_begin':
    case 'patch_apply_begin':
    case 'dynamic_tool_call_request':
      return 'tool.started'
    case 'exec_command_output_delta':
    case 'terminal_interaction':
    case 'patch_apply_updated':
      return 'tool.output'
    case 'exec_command_end':
    case 'mcp_tool_call_end':
    case 'web_search_end':
    case 'image_generation_end':
    case 'patch_apply_end':
    case 'dynamic_tool_call_response':
      return 'tool.completed'
    case 'exec_approval_request':
    case 'apply_patch_approval_request':
    case 'request_permissions':
    case 'request_user_input':
    case 'elicitation_request':
      return 'approval.requested'
    case 'turn_diff':
      return 'diff.updated'
    case 'token_count':
    case 'raw_response_completed':
      return 'usage.updated'
    case 'collab_agent_spawn_begin':
      return 'subagent.started'
    case 'collab_agent_spawn_end':
    case 'collab_agent_interaction_end':
    case 'collab_waiting_end':
    case 'collab_close_end':
    case 'collab_resume_end':
      return 'subagent.completed'
    case 'collab_agent_interaction_begin':
    case 'collab_waiting_begin':
    case 'collab_close_begin':
    case 'collab_resume_begin':
    case 'sub_agent_activity':
      return 'subagent.updated'
    case 'error':
    case 'stream_error':
    case 'serialization_error':
      return 'failure'
    case 'warning':
    case 'guardian_warning':
      return 'warning'
    default:
      return 'notice'
  }
}

function statusTerminalReason(
  kind: RuntimeEventKind,
  message: Record<string, unknown>,
): RuntimeTerminalReason | undefined {
  if (kind === 'turn.completed') return isRecord(message.error) ? 'failed' : 'completed'
  if (kind === 'turn.aborted') return 'aborted'
  if (kind === 'failure') return 'failed'
  if (kind !== 'tool.completed' && kind !== 'subagent.completed') return undefined
  const item = nestedRecord(message, 'item')
  const rawStatus = nonEmptyString(message.status) ?? nonEmptyString(item?.status)
  const status = rawStatus?.toLowerCase()
  if (status === 'failed' || message.success === false || item?.success === false) return 'failed'
  if (status === 'declined' || status === 'denied') return 'declined'
  if (status === 'cancelled' || status === 'interrupted') return 'cancelled'
  if (status === 'completed' || message.success === true || item?.success === true) return 'completed'
  return 'unknown'
}

function eventTimeMillis(message: Record<string, unknown>): number | undefined {
  for (const key of ['completed_at_ms', 'occurred_at_ms', 'started_at_ms']) {
    const value = message[key]
    if (typeof value === 'number' && Number.isSafeInteger(value) && value >= 0) return value
  }
  for (const key of ['completed_at', 'started_at']) {
    const value = message[key]
    if (typeof value === 'number' && Number.isSafeInteger(value) && value >= 0) {
      return value * 1_000
    }
  }
  return undefined
}

function serializedAgentPath(value: unknown): string | undefined {
  if (typeof value === 'string' && value.length > 0) return value
  if (value === undefined || value === null) return undefined
  const serialized = JSON.stringify(value)
  return serialized === undefined ? undefined : serialized
}

function sourceIdentity(
  sessionId: string,
  kernelSessionId: string,
  roleId: string,
  kernelStreamId: string,
  kernelSequence: string,
  envelope: Record<string, unknown>,
  message: Record<string, unknown>,
  kernelKind: string,
): RuntimeSourceIdentity {
  const item = nestedRecord(message, 'item')
  const submissionId = nonEmptyString(envelope.id)
    ?? `kernel:${kernelStreamId}:${kernelSequence}`
  const turnId = nonEmptyString(message.turn_id)
  const itemId = nonEmptyString(message.item_id) ?? nonEmptyString(item?.id)
  const toolCallId = nonEmptyString(message.call_id)
    ?? (isToolItem(nonEmptyString(item?.type)) ? nonEmptyString(item?.id) : undefined)
  const approvalId = nonEmptyString(message.approval_id)
    ?? (normalizedKind(nonEmptyString(message.type) ?? kernelKind, message) === 'approval.requested'
      ? nonEmptyString(message.call_id) ?? nonEmptyString(message.id)
      : undefined)
  const agentThreadId = nonEmptyString(message.agent_thread_id)
    ?? nonEmptyString(message.new_thread_id)
    ?? nonEmptyString(message.receiver_thread_id)
    ?? nonEmptyString(item?.agent_thread_id)
  const agentPath = serializedAgentPath(message.agent_path ?? item?.agent_path)
  return Object.freeze({
    authority: 'codex-core',
    sessionId,
    kernelSessionId,
    roleId,
    kernelStreamId,
    kernelSequence,
    submissionId,
    kernelKind,
    ...(turnId === undefined ? {} : { turnId }),
    ...(itemId === undefined ? {} : { itemId }),
    ...(toolCallId === undefined ? {} : { toolCallId }),
    ...(approvalId === undefined ? {} : { approvalId }),
    ...(agentThreadId === undefined ? {} : { agentThreadId }),
    ...(agentPath === undefined ? {} : { agentPath }),
  })
}

function parseEnvelope(event: KernelEvent, sessionId: string): {
  readonly envelope: Record<string, unknown>
  readonly message: Record<string, unknown>
  readonly type: string
} {
  let parsed: unknown
  try {
    parsed = JSON.parse(event.rawJson) as unknown
  } catch (error) {
    throw new RuntimeProjectionError(
      'INVALID_KERNEL_EVENT',
      `kernel event ${event.sequence.toString()} is not valid JSON`,
      { sessionId, actualSequence: event.sequence.toString() },
    )
  }
  if (!isRecord(parsed)) {
    throw new RuntimeProjectionError(
      'INVALID_KERNEL_EVENT',
      `kernel event ${event.sequence.toString()} is not an object`,
      { sessionId, actualSequence: event.sequence.toString() },
    )
  }
  const nestedMessage = nestedRecord(parsed, 'msg')
  if (nestedMessage !== undefined) {
    const type = nonEmptyString(nestedMessage.type)
    if (type !== undefined) return { envelope: parsed, message: nestedMessage, type }
  }
  const type = nonEmptyString(parsed.type)
  if (type !== undefined && (type === 'stream_error' || type === 'serialization_error')) {
    return { envelope: parsed, message: parsed, type }
  }
  throw new RuntimeProjectionError(
    'INVALID_KERNEL_EVENT',
    `kernel event ${event.sequence.toString()} has no Codex message envelope`,
    { sessionId, actualSequence: event.sequence.toString() },
  )
}

/** Strict one-session normalizer shared by live delivery and persisted replay. */
export class CodexRuntimeProjector {
  readonly sessionId: string
  readonly kernelSessionId: string
  readonly roleId: string
  readonly kernelStreamId: string
  readonly rememberedEventLimit: number
  #sequence: bigint
  #kernelSequence: bigint
  readonly #fingerprints = new Map<string, string>()

  constructor(options: CodexRuntimeProjectorOptions) {
    try {
      if (options.sessionId.length === 0) throw new Error('sessionId must not be empty')
      if (options.kernelSessionId?.length === 0) {
        throw new Error('kernelSessionId must not be empty')
      }
      if (options.roleId.length === 0) throw new Error('roleId must not be empty')
      if (options.kernelStreamId.length === 0) throw new Error('kernelStreamId must not be empty')
      this.sessionId = options.sessionId
      this.kernelSessionId = options.kernelSessionId ?? options.sessionId
      this.roleId = options.roleId
      this.kernelStreamId = options.kernelStreamId
      this.#sequence = sequenceValue(options.startAfterSequence, 'startAfterSequence')
      this.#kernelSequence = sequenceValue(
        options.kernelStartAfterSequence,
        'kernelStartAfterSequence',
      )
      this.rememberedEventLimit = boundedLimit(options.rememberedEventLimit)
    } catch (error) {
      const message = error instanceof Error ? error.message : 'invalid projector options'
      throw new RuntimeProjectionError('INVALID_PROJECTOR_OPTIONS', message, {
        sessionId: options.sessionId,
      })
    }
  }

  get cursor(): RuntimeCursor {
    return Object.freeze({ sessionId: this.sessionId, sequence: this.#sequence.toString() })
  }

  get checkpoint(): CodexRuntimeCheckpoint {
    return Object.freeze({
      cursor: this.cursor,
      kernelStreamId: this.kernelStreamId,
      kernelSequence: this.#kernelSequence.toString(),
    })
  }

  /** Project one kernel event, returning undefined for a verified recent duplicate. */
  ingest(event: KernelEvent): RuntimeEvent | undefined {
    const actual = event.sequence
    const expected = this.#kernelSequence + 1n
    const actualText = actual.toString()
    const expectedText = expected.toString()
    const fingerprint = `${event.kind}\u0000${event.rawJson}`
    if (actual < expected) {
      const prior = this.#fingerprints.get(actualText)
      if (prior === fingerprint) return undefined
      const code = prior === undefined ? 'EVENT_SEQUENCE_OUT_OF_ORDER' : 'EVENT_SEQUENCE_CONFLICT'
      throw new RuntimeProjectionError(
        code,
        prior === undefined
          ? `kernel event ${actualText} arrived after native cursor ${this.#kernelSequence.toString()}`
          : `kernel event ${actualText} changed after it was projected`,
        { sessionId: this.sessionId, expectedSequence: expectedText, actualSequence: actualText },
      )
    }
    if (actual > expected) {
      throw new RuntimeProjectionError(
        'EVENT_SEQUENCE_MISSING',
        `kernel event ${expectedText} is missing before ${actualText}`,
        { sessionId: this.sessionId, expectedSequence: expectedText, actualSequence: actualText },
      )
    }

    const { envelope, message, type } = parseEnvelope(event, this.sessionId)
    const kind = normalizedKind(type, message)
    const sequence = (this.#sequence + 1n).toString()
    const source = sourceIdentity(
      this.sessionId,
      this.kernelSessionId,
      this.roleId,
      this.kernelStreamId,
      actualText,
      envelope,
      message,
      event.kind,
    )
    const occurredAtMillis = eventTimeMillis(message)
    const terminalReason = statusTerminalReason(kind, message)
    const normalized: RuntimeEvent = Object.freeze({
      schemaVersion: RUNTIME_EVENT_SCHEMA_VERSION,
      id: runtimeEventId(this.sessionId, sequence),
      cursor: Object.freeze({ sessionId: this.sessionId, sequence }),
      kind,
      source,
      ...(occurredAtMillis === undefined ? {} : { occurredAtMillis }),
      ...(terminalReason === undefined ? {} : { terminalReason }),
      data: freezeRecord(message),
    })
    this.#sequence += 1n
    this.#kernelSequence = actual
    this.#fingerprints.set(actualText, fingerprint)
    while (this.#fingerprints.size > this.rememberedEventLimit) {
      const oldest = this.#fingerprints.keys().next().value as string | undefined
      if (oldest === undefined) break
      this.#fingerprints.delete(oldest)
    }
    return normalized
  }

  /** Replay a contiguous range through the exact live transition. */
  replay(events: Iterable<KernelEvent>): readonly RuntimeEvent[] {
    const projected: RuntimeEvent[] = []
    for (const event of events) {
      const normalized = this.ingest(event)
      if (normalized !== undefined) projected.push(normalized)
    }
    return Object.freeze(projected)
  }
}

/** Convenience fold used by cold rebuild paths and deterministic tests. */
export function projectKernelEvents(
  options: CodexRuntimeProjectorOptions,
  events: Iterable<KernelEvent>,
): readonly RuntimeEvent[] {
  return new CodexRuntimeProjector(options).replay(events)
}

/** Live normalized stream with no queue beyond the native kernel's bounded channel. */
export async function* streamRuntimeEvents(
  source: KernelEventSource,
  options: RuntimeEventStreamOptions,
): AsyncGenerator<RuntimeEvent, CodexRuntimeCheckpoint, undefined> {
  const projector = new CodexRuntimeProjector(options)
  const streamOptions = {
    ...(options.signal === undefined ? {} : { signal: options.signal }),
    ...(options.timeoutMillis === undefined ? {} : { timeoutMillis: options.timeoutMillis }),
  }
  for await (const event of source.events(options.kernelSessionId ?? options.sessionId, streamOptions)) {
    const projected = projector.ingest(event)
    if (projected !== undefined) yield projected
  }
  return projector.checkpoint
}
