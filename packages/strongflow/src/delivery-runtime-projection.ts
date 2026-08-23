import {
  RUNTIME_EVENT_SCHEMA_VERSION,
  parseDelivery,
  runtimeEventId,
  type AttentionItemType,
  type Delivery,
  type DeliveryId,
  type DeliveryTask,
  type DeliveryTaskId,
  type EvidenceRefType,
  type RuntimeAgentGraphChange,
  type RuntimeEvent,
  type RuntimeEventKind,
  type RuntimeInputQuestion,
  type SessionBinding,
  type StageRun,
  type StageRunId,
} from '@winwincode/contracts'

export const DELIVERY_RUNTIME_PROJECTION_SCHEMA_VERSION = 1 as const

const DEFAULT_REMEMBERED_EVENT_LIMIT = 2_048
const MAX_REMEMBERED_EVENT_LIMIT = 65_536

export type DeliveryRuntimeProjectionErrorCode =
  | 'INVALID_PROJECTION_OPTIONS'
  | 'INVALID_RUNTIME_EVENT'
  | 'RUNTIME_SESSION_UNBOUND'
  | 'RUNTIME_SESSION_AMBIGUOUS'
  | 'RUNTIME_SEQUENCE_MISSING'
  | 'RUNTIME_SEQUENCE_CONFLICT'
  | 'RUNTIME_SEQUENCE_OUT_OF_ORDER'

export class DeliveryRuntimeProjectionError extends Error {
  readonly code: DeliveryRuntimeProjectionErrorCode

  constructor(code: DeliveryRuntimeProjectionErrorCode, message: string, options?: ErrorOptions) {
    super(message, options)
    this.name = 'DeliveryRuntimeProjectionError'
    this.code = code
  }
}

export interface DeliveryRuntimeEventLink {
  readonly eventId: string
  readonly sourceRef: string
  readonly sessionId: string
  readonly kernelSessionId: string
  readonly sequence: string
  readonly kind: RuntimeEventKind
}

export interface DeliveryRuntimePlanItem {
  readonly step: string
  readonly status: 'pending' | 'in_progress' | 'completed'
}

export interface DeliveryRuntimePlan {
  readonly itemId: string | null
  readonly explanation: string | null
  readonly items: readonly DeliveryRuntimePlanItem[]
  readonly text: string | null
  readonly complete: boolean
  readonly firstEvent: DeliveryRuntimeEventLink
  readonly latestEvent: DeliveryRuntimeEventLink
}

export type DeliveryRuntimeAgentStatus =
  | 'unknown'
  | 'waiting'
  | 'running'
  | 'completed'
  | 'interrupted'
  | 'failed'
  | 'closed'

export interface DeliveryRuntimeAgentNode {
  readonly threadId: string
  readonly path: string | null
  readonly parentThreadId: string | null
  readonly nickname: string | null
  readonly role: string | null
  readonly status: DeliveryRuntimeAgentStatus
  readonly firstEvent: DeliveryRuntimeEventLink
  readonly latestEvent: DeliveryRuntimeEventLink
}

export interface DeliveryRuntimeAgentEdge {
  readonly parentThreadId: string
  readonly childThreadId: string
}

export type DeliveryRuntimeActivityStatus =
  | 'running'
  | 'completed'
  | 'failed'
  | 'declined'
  | 'cancelled'
  | 'unknown'

export type DeliveryRuntimeEvidenceOutcome =
  | 'observed'
  | 'succeeded'
  | 'task-failed'
  | 'timed-out'
  | 'policy-denied'
  | 'infrastructure-failed'
  | 'cancelled'

export interface DeliveryRuntimeActivity {
  readonly callId: string
  readonly activityType: 'command' | 'test'
  readonly command: string | null
  readonly status: DeliveryRuntimeActivityStatus
  readonly outcome: DeliveryRuntimeEvidenceOutcome
  readonly exitCode: number | null
  readonly firstEvent: DeliveryRuntimeEventLink
  readonly latestEvent: DeliveryRuntimeEventLink
}

export interface DeliveryRuntimeInteraction {
  readonly id: string
  readonly operationId: string
  readonly interactionType: 'execution-approval' | 'user-input'
  readonly blocking: boolean
  readonly status: 'pending' | 'resolved'
  readonly questions: readonly RuntimeInputQuestion[]
  readonly requestedEvent: DeliveryRuntimeEventLink
  readonly resolvedEvent: DeliveryRuntimeEventLink | null
}

export interface DeliveryRuntimeFailure {
  readonly message: string
  readonly code: string | null
  readonly event: DeliveryRuntimeEventLink
}

export interface DeliveryRuntimeRecovery {
  readonly state: 'none' | 'required' | 'in-progress' | 'recovered'
  readonly failureCount: number
  readonly recoveryCount: number
  readonly lastFailureEvent: DeliveryRuntimeEventLink | null
  readonly latestRecoveryEvent: DeliveryRuntimeEventLink | null
}

export interface DeliveryRuntimeDiff {
  readonly unifiedDiff: string
  readonly changedFiles: readonly string[]
  readonly additions: number
  readonly deletions: number
  readonly event: DeliveryRuntimeEventLink
}

export interface DeliveryRuntimeUsage {
  readonly totals: Readonly<Record<string, number>>
  readonly event: DeliveryRuntimeEventLink
}

export interface DeliveryRuntimeEvidenceLink {
  readonly type: Exclude<EvidenceRefType, 'pull_request'>
  readonly outcome: DeliveryRuntimeEvidenceOutcome
  readonly sourceRef: string
  readonly stageRunId: StageRunId
  readonly sessionBindingId: string
  readonly eventId: string
}

export interface DeliveryRuntimeAttentionCandidate {
  readonly type: AttentionItemType
  readonly title: string
  readonly blocking: boolean
  readonly status: 'open' | 'resolved'
  readonly stageRunId: StageRunId
  readonly sessionBindingId: string
  readonly sourceRef: string
  readonly questions: readonly RuntimeInputQuestion[]
}

export interface DeliverySessionRuntimeView {
  readonly binding: SessionBinding
  readonly asOfSequence: string
  readonly plan: DeliveryRuntimePlan | null
  readonly agents: readonly DeliveryRuntimeAgentNode[]
  readonly agentEdges: readonly DeliveryRuntimeAgentEdge[]
  readonly activities: readonly DeliveryRuntimeActivity[]
  readonly interactions: readonly DeliveryRuntimeInteraction[]
  readonly failures: readonly DeliveryRuntimeFailure[]
  readonly recovery: DeliveryRuntimeRecovery
  readonly diff: DeliveryRuntimeDiff | null
  readonly usage: DeliveryRuntimeUsage | null
  readonly evidenceLinks: readonly DeliveryRuntimeEvidenceLink[]
  readonly attentionCandidates: readonly DeliveryRuntimeAttentionCandidate[]
}

export interface DeliveryStageRuntimeView {
  readonly stageRun: StageRun
  readonly sessions: readonly DeliverySessionRuntimeView[]
  readonly changedFiles: readonly string[]
  readonly evidenceLinks: readonly DeliveryRuntimeEvidenceLink[]
  readonly attentionCandidates: readonly DeliveryRuntimeAttentionCandidate[]
}

export interface DeliveryTaskRuntimeView {
  readonly deliveryTask: DeliveryTask
  readonly deliveryTaskId: DeliveryTaskId
  readonly stageRunIds: readonly StageRunId[]
  readonly changedFiles: readonly string[]
  readonly evidenceLinks: readonly DeliveryRuntimeEvidenceLink[]
}

export interface DeliveryRuntimeProjectionSnapshot {
  readonly schemaVersion: typeof DELIVERY_RUNTIME_PROJECTION_SCHEMA_VERSION
  readonly deliveryId: DeliveryId
  readonly deliveryRevision: number
  readonly stages: readonly DeliveryStageRuntimeView[]
  readonly tasks: readonly DeliveryTaskRuntimeView[]
}

export interface DeliveryRuntimeProjectionResult {
  readonly changed: boolean
}

export interface DeliveryRuntimeProjectionOptions {
  readonly delivery: Delivery
  /** Recent event identities retained only to recognize immediate replay duplicates. */
  readonly rememberedEventLimit?: number
}

interface MutableSessionState {
  readonly binding: SessionBinding
  readonly stageRun: StageRun
  sequence: bigint
  readonly fingerprints: Map<string, string>
  plan: DeliveryRuntimePlan | null
  readonly agents: Map<string, DeliveryRuntimeAgentNode>
  readonly activities: Map<string, DeliveryRuntimeActivity>
  readonly interactions: Map<string, DeliveryRuntimeInteraction>
  readonly failures: DeliveryRuntimeFailure[]
  recovery: DeliveryRuntimeRecovery
  diff: DeliveryRuntimeDiff | null
  usage: DeliveryRuntimeUsage | null
  readonly evidence: Map<string, DeliveryRuntimeEvidenceLink>
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

function nonEmptyString(value: unknown): string | undefined {
  return typeof value === 'string' && value.length > 0 ? value : undefined
}

function nestedRecord(
  record: Readonly<Record<string, unknown>>,
  key: string,
): Record<string, unknown> | undefined {
  const value = record[key]
  return isRecord(value) ? value : undefined
}

function immutable<Value>(value: Value): Value {
  const clone = structuredClone(value)
  const pending: object[] = []
  if (typeof clone === 'object' && clone !== null) pending.push(clone)
  while (pending.length > 0) {
    const current = pending.pop()!
    if (Object.isFrozen(current)) continue
    Object.freeze(current)
    for (const child of Object.values(current)) {
      if (typeof child === 'object' && child !== null) pending.push(child)
    }
  }
  return clone
}

function eventLink(event: RuntimeEvent): DeliveryRuntimeEventLink {
  return Object.freeze({
    eventId: event.id,
    sourceRef: `runtime_event:${event.id}`,
    sessionId: event.source.sessionId,
    kernelSessionId: event.source.kernelSessionId,
    sequence: event.cursor.sequence,
    kind: event.kind,
  })
}

function eventFingerprint(event: RuntimeEvent): string {
  return JSON.stringify(event)
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

function commandText(value: unknown): string | null {
  if (typeof value === 'string' && value.length > 0) return value
  if (!Array.isArray(value)) return null
  const parts = value.filter((part): part is string => typeof part === 'string')
  return parts.length === 0 ? null : parts.join(' ')
}

function commandFrom(event: RuntimeEvent): string | null {
  const item = nestedRecord(event.data, 'item')
  return commandText(item?.command) ?? commandText(event.data.command)
}

function isCommandEvent(event: RuntimeEvent): boolean {
  const item = nestedRecord(event.data, 'item')
  return item?.type === 'CommandExecution'
    || nonEmptyString(event.data.type)?.startsWith('exec_command_') === true
}

function isTestCommand(command: string | null): boolean {
  if (command === null) return false
  return /(?:^|[\s;&|])(?:cargo\s+test|go\s+test|pytest|vitest|jest|node\s+--test|(?:npm|pnpm|yarn|bun)\s+(?:run\s+)?test(?=$|[\s:]))/iu.test(command)
}

function exitCode(event: RuntimeEvent): number | null {
  const item = nestedRecord(event.data, 'item')
  const result = nestedRecord(event.data, 'result')
  const evidence = nestedRecord(event.data, 'evidence')
  for (const value of [
    item?.exit_code,
    result?.exit_code,
    result?.exitCode,
    evidence?.exit_code,
    evidence?.exitCode,
    event.data.exit_code,
    event.data.exitCode,
  ]) {
    if (typeof value === 'number' && Number.isSafeInteger(value)) return value
  }
  return null
}

function normalizedOutcomeToken(value: unknown): string | undefined {
  return typeof value === 'string' && value.length > 0
    ? value.toLowerCase().replaceAll('_', '-')
    : undefined
}

function runtimeStatus(event: RuntimeEvent): string | undefined {
  const item = nestedRecord(event.data, 'item')
  const result = nestedRecord(event.data, 'result')
  const evidence = nestedRecord(event.data, 'evidence')
  for (const value of [
    evidence?.outcome,
    evidence?.status,
    result?.status,
    item?.status,
    event.data.status,
  ]) {
    const status = normalizedOutcomeToken(value)
    if (status !== undefined) return status
  }
  return undefined
}

function failureInfoToken(value: unknown): string | undefined {
  if (typeof value === 'string' && value.length > 0) return value
  if (!isRecord(value)) return undefined
  for (const candidate of [value.type, value.code]) {
    if (typeof candidate === 'string' && candidate.length > 0) return candidate
  }
  const keys = Object.keys(value)
  return keys.length === 1 ? keys[0] : undefined
}

function runtimeFailureCode(event: RuntimeEvent): string | null {
  const error = nestedRecord(event.data, 'error')
  for (const value of [
    error?.code,
    event.data.code,
    error?.codex_error_info,
    event.data.codex_error_info,
  ]) {
    const token = failureInfoToken(value)
    if (token !== undefined) return token
  }
  return null
}

function runtimeBoolean(event: RuntimeEvent, snakeCase: string, camelCase: string): boolean {
  const item = nestedRecord(event.data, 'item')
  const result = nestedRecord(event.data, 'result')
  const evidence = nestedRecord(event.data, 'evidence')
  return [
    evidence?.[snakeCase],
    evidence?.[camelCase],
    result?.[snakeCase],
    result?.[camelCase],
    item?.[snakeCase],
    item?.[camelCase],
    event.data[snakeCase],
    event.data[camelCase],
  ].some(value => value === true)
}

function runtimeFailureText(event: RuntimeEvent): string {
  const item = nestedRecord(event.data, 'item')
  const result = nestedRecord(event.data, 'result')
  const error = nestedRecord(event.data, 'error')
  return [
    error?.message,
    result?.message,
    item?.formatted_output,
    item?.formattedOutput,
    item?.stderr,
    result?.formatted_output,
    result?.formattedOutput,
    result?.stderr,
    event.data.message,
    event.data.formatted_output,
    event.data.formattedOutput,
    event.data.stderr,
  ].filter((value): value is string => typeof value === 'string').join('\n').toLowerCase()
}

/** Derive one explainable outcome from a Codex-owned source fact. */
export function deliveryRuntimeEvidenceOutcome(
  event: RuntimeEvent,
  type: Exclude<EvidenceRefType, 'pull_request'>,
): DeliveryRuntimeEvidenceOutcome {
  const status = runtimeStatus(event)
  const code = runtimeFailureCode(event)?.toLowerCase().replaceAll('_', '-') ?? ''
  const failureText = runtimeFailureText(event)
  if (status === 'timed-out'
    || status === 'timeout'
    || code.includes('timeout')
    || code.includes('deadline')
    || runtimeBoolean(event, 'timed_out', 'timedOut')
    || failureText.includes('command timed out')
    || failureText.includes('request timed out')) {
    return 'timed-out'
  }
  if (status === 'sandbox-denied'
    || status === 'policy-denied'
    || status === 'declined'
    || status === 'denied'
    || event.terminalReason === 'declined'
    || code.includes('policy-denied')
    || code === 'cyber-policy'
    || code === 'misalignment-policy-violation'
    || failureText.includes('sandbox denied')
    || failureText.includes('denied by policy')) return 'policy-denied'
  if (status === 'cancelled'
    || status === 'canceled'
    || status === 'interrupted'
    || event.terminalReason === 'cancelled'
    || event.terminalReason === 'aborted') return 'cancelled'
  if (event.kind === 'failure'
    || status === 'output-limit'
    || code.includes('enforcement-unavailable')
    || code.includes('infrastructure')
    || code.includes('native-protocol')
    || code.includes('transport')
    || code.includes('rate-limit')
    || code === 'server'
    || code === 'empty-response') return 'infrastructure-failed'
  const commandFact = type === 'command' || type === 'test'
  if (commandFact
    && (status === 'failed'
      || event.terminalReason === 'failed'
      || (exitCode(event) !== null && exitCode(event) !== 0))) return 'task-failed'
  if (commandFact
    && (status === 'completed'
      || status === 'exited'
      || event.terminalReason === 'completed'
      || exitCode(event) === 0)) return 'succeeded'
  return 'observed'
}

function activityStatus(
  event: RuntimeEvent,
  type: DeliveryRuntimeActivity['activityType'],
): DeliveryRuntimeActivityStatus {
  if (event.kind === 'tool.started' || event.kind === 'tool.output') return 'running'
  switch (deliveryRuntimeEvidenceOutcome(event, type)) {
    case 'succeeded': return 'completed'
    case 'task-failed':
    case 'timed-out':
    case 'infrastructure-failed': return 'failed'
    case 'policy-denied': return 'declined'
    case 'cancelled': return 'cancelled'
    case 'observed': return 'unknown'
  }
}

function diffPath(line: string): string | undefined {
  const value = line.slice(4).split('\t', 1)[0]?.trim()
  if (value === undefined || value.length === 0 || value === '/dev/null') return undefined
  const unquoted = value.startsWith('"') && value.endsWith('"')
    ? value.slice(1, -1)
    : value
  return unquoted.startsWith('a/') || unquoted.startsWith('b/') ? unquoted.slice(2) : unquoted
}

function diffSummary(event: RuntimeEvent): DeliveryRuntimeDiff | null {
  const unifiedDiff = nonEmptyString(event.data.unified_diff)
  if (unifiedDiff === undefined) return null
  const files = new Set<string>()
  let additions = 0
  let deletions = 0
  for (const line of unifiedDiff.split('\n')) {
    if (line.startsWith('+++ ') || line.startsWith('--- ')) {
      const path = diffPath(line)
      if (path !== undefined) files.add(path)
    } else if (line.startsWith('+')) additions += 1
    else if (line.startsWith('-')) deletions += 1
  }
  return Object.freeze({
    unifiedDiff,
    changedFiles: Object.freeze([...files].sort()),
    additions,
    deletions,
    event: eventLink(event),
  })
}

function numericRecord(value: unknown): Readonly<Record<string, number>> {
  if (!isRecord(value)) return Object.freeze({})
  return Object.freeze(Object.fromEntries(Object.entries(value).filter(
    (entry): entry is [string, number] => typeof entry[1] === 'number'
      && Number.isFinite(entry[1]),
  )))
}

function usageSummary(event: RuntimeEvent): DeliveryRuntimeUsage {
  const info = nestedRecord(event.data, 'info')
  const totals = numericRecord(info?.total_token_usage ?? event.data.total_token_usage)
  return Object.freeze({ totals, event: eventLink(event) })
}

function failureMessage(event: RuntimeEvent): string {
  const error = nestedRecord(event.data, 'error')
  return nonEmptyString(event.data.message)
    ?? nonEmptyString(error?.message)
    ?? `${event.kind} ended with ${event.terminalReason ?? 'an unknown failure'}`
}

function failureCode(event: RuntimeEvent): string | null {
  return runtimeFailureCode(event)
}

function isFailureEvent(event: RuntimeEvent): boolean {
  return event.kind === 'failure'
    || event.kind === 'turn.aborted'
    || event.terminalReason === 'failed'
    || event.terminalReason === 'aborted'
}

function explicitEvidenceType(event: RuntimeEvent): DeliveryRuntimeEvidenceLink['type'] | null {
  if (event.semantic?.kind === 'verification-result') return 'review_finding'
  const item = nestedRecord(event.data, 'item')
  const evidence = nestedRecord(event.data, 'evidence')
  const value = nonEmptyString(evidence?.type)
    ?? nonEmptyString(item?.evidence_type)
    ?? nonEmptyString(event.data.evidence_type)
  if (value === 'test'
    || value === 'command'
    || value === 'diff'
    || value === 'file'
    || value === 'commit'
    || value === 'runtime_event'
    || value === 'review_finding') return value
  if (event.kind === 'tool.completed' && item?.type === 'FileChange') return 'file'
  return null
}

function graphStatus(value: string): DeliveryRuntimeAgentStatus {
  switch (value) {
    case 'waiting': return 'waiting'
    case 'running': return 'running'
    case 'completed': return 'completed'
    case 'interrupted': return 'interrupted'
    case 'failed': return 'failed'
    case 'closed': return 'closed'
    default: return 'unknown'
  }
}

function attentionType(stageRun: StageRun): AttentionItemType {
  switch (stageRun.stage) {
    case 'clarifying': return 'requirement_question'
    case 'plan-review': return 'decision_required'
    case 'verifying': return 'verification_blocked'
    case 'delivery-review': return 'delivery_approval'
    case 'planning':
    case 'executing':
    case 'reworking': return 'decision_required'
  }
}

function attentionTitle(questions: readonly RuntimeInputQuestion[]): string {
  return questions[0]?.header || questions[0]?.question || 'Agent input required'
}

function uniqueBy<Value>(values: readonly Value[], key: (value: Value) => string): readonly Value[] {
  const selected = new Map<string, Value>()
  for (const value of values) selected.set(key(value), value)
  return Object.freeze([...selected.values()])
}

/**
 * Rebuildable, in-memory view over Codex-owned RuntimeEvents. It never writes
 * Delivery state, RuntimeSessionLedger records, or Codex execution state.
 */
export class DeliveryRuntimeProjection {
  readonly delivery: Delivery
  readonly rememberedEventLimit: number
  readonly #states = new Map<string, MutableSessionState>()

  constructor(options: DeliveryRuntimeProjectionOptions) {
    try {
      if (!isRecord(options)) throw new Error('projection options must be an object')
      this.delivery = parseDelivery(options.delivery)
      this.rememberedEventLimit = boundedLimit(options.rememberedEventLimit)
      const runs = new Map(this.delivery.stageRuns.map(run => [run.id, run]))
      const claimedDshSessions = new Set<string>()
      const claimedCodexSessions = new Set<string>()
      for (const binding of this.delivery.sessionBindings) {
        if (binding.dshSessionId !== null) {
          if (claimedDshSessions.has(binding.dshSessionId)) {
            throw new Error(`DSH session ${binding.dshSessionId} is bound more than once`)
          }
          claimedDshSessions.add(binding.dshSessionId)
        }
        if (binding.codexSessionId !== null) {
          if (claimedCodexSessions.has(binding.codexSessionId)) {
            throw new Error(`Codex session ${binding.codexSessionId} is bound more than once`)
          }
          claimedCodexSessions.add(binding.codexSessionId)
        }
        const stageRun = runs.get(binding.stageRunId)
        if (stageRun === undefined) throw new Error(`StageRun ${binding.stageRunId} is missing`)
        this.#states.set(binding.id, {
          binding,
          stageRun,
          sequence: 0n,
          fingerprints: new Map(),
          plan: null,
          agents: new Map(),
          activities: new Map(),
          interactions: new Map(),
          failures: [],
          recovery: Object.freeze({
            state: 'none',
            failureCount: 0,
            recoveryCount: 0,
            lastFailureEvent: null,
            latestRecoveryEvent: null,
          }),
          diff: null,
          usage: null,
          evidence: new Map(),
        })
      }
    } catch (error) {
      if (error instanceof DeliveryRuntimeProjectionError) throw error
      throw new DeliveryRuntimeProjectionError(
        'INVALID_PROJECTION_OPTIONS',
        'Delivery runtime projection options are invalid',
        { cause: error },
      )
    }
  }

  get snapshot(): DeliveryRuntimeProjectionSnapshot {
    const sessionsByRun = new Map<string, DeliverySessionRuntimeView[]>()
    for (const state of this.#states.values()) {
      const sessions = sessionsByRun.get(state.stageRun.id) ?? []
      sessions.push(this.#sessionSnapshot(state))
      sessionsByRun.set(state.stageRun.id, sessions)
    }
    const stages = this.delivery.stageRuns.map((stageRun) => {
      const sessions = (sessionsByRun.get(stageRun.id) ?? []).sort((left, right) => (
        left.binding.id.localeCompare(right.binding.id)
      ))
      return {
        stageRun,
        sessions,
        changedFiles: [...new Set(sessions.flatMap(session => (
          session.diff?.changedFiles ?? []
        )))].sort(),
        evidenceLinks: uniqueBy(
          sessions.flatMap(session => session.evidenceLinks),
          link => `${link.sourceRef}\u0000${link.type}`,
        ),
        attentionCandidates: uniqueBy(
          sessions.flatMap(session => session.attentionCandidates),
          candidate => candidate.sourceRef,
        ),
      }
    })
    const tasks = this.delivery.tasks.map((task) => {
      const taskStages = stages.filter(stage => stage.stageRun.deliveryTaskId === task.id)
      return {
        deliveryTask: task,
        deliveryTaskId: task.id,
        stageRunIds: taskStages.map(stage => stage.stageRun.id),
        changedFiles: [...new Set(taskStages.flatMap(stage => stage.changedFiles))].sort(),
        evidenceLinks: uniqueBy(
          taskStages.flatMap(stage => stage.evidenceLinks),
          link => `${link.sourceRef}\u0000${link.type}`,
        ),
      }
    })
    return immutable({
      schemaVersion: DELIVERY_RUNTIME_PROJECTION_SCHEMA_VERSION,
      deliveryId: this.delivery.id,
      deliveryRevision: this.delivery.revision,
      stages,
      tasks,
    })
  }

  apply(event: RuntimeEvent): DeliveryRuntimeProjectionResult {
    const state = this.#stateFor(event)
    const actual = BigInt(event.cursor.sequence)
    const expected = state.sequence + 1n
    const fingerprint = eventFingerprint(event)
    if (actual < expected) {
      const previous = state.fingerprints.get(event.id)
      if (previous === fingerprint) return Object.freeze({ changed: false })
      throw new DeliveryRuntimeProjectionError(
        previous === undefined ? 'RUNTIME_SEQUENCE_OUT_OF_ORDER' : 'RUNTIME_SEQUENCE_CONFLICT',
        previous === undefined
          ? `runtime event ${event.id} arrived behind sequence ${state.sequence.toString()}`
          : `runtime event ${event.id} changed after projection`,
      )
    }
    if (actual > expected) {
      throw new DeliveryRuntimeProjectionError(
        'RUNTIME_SEQUENCE_MISSING',
        `runtime sequence ${expected.toString()} is missing before ${actual.toString()}`,
      )
    }

    this.#applyPlan(state, event)
    this.#applyAgents(state, event)
    this.#applyActivity(state, event)
    this.#applyInteraction(state, event)
    this.#applyFailureAndRecovery(state, event)
    if (event.kind === 'diff.updated') {
      state.diff = diffSummary(event)
      if (state.diff !== null) this.#addEvidence(state, event, 'diff')
    }
    const explicitType = explicitEvidenceType(event)
    if (explicitType !== null) this.#addEvidence(state, event, explicitType)
    if (event.kind === 'usage.updated') state.usage = usageSummary(event)
    state.sequence = actual
    state.fingerprints.set(event.id, fingerprint)
    while (state.fingerprints.size > this.rememberedEventLimit) {
      const oldest = state.fingerprints.keys().next().value as string | undefined
      if (oldest === undefined) break
      state.fingerprints.delete(oldest)
    }
    return Object.freeze({ changed: true })
  }

  replay(events: Iterable<RuntimeEvent>): DeliveryRuntimeProjectionSnapshot {
    for (const event of events) this.apply(event)
    return this.snapshot
  }

  #stateFor(event: RuntimeEvent): MutableSessionState {
    if (!isRecord(event)
      || event.schemaVersion !== RUNTIME_EVENT_SCHEMA_VERSION
      || event.source?.authority !== 'codex-core'
      || event.cursor?.sessionId !== event.source.sessionId
      || !/^\d+$/u.test(event.cursor.sequence)
      || event.id !== runtimeEventId(event.source.sessionId, event.cursor.sequence)
      || !isRecord(event.data)) {
      throw new DeliveryRuntimeProjectionError(
        'INVALID_RUNTIME_EVENT',
        'Delivery runtime projection received an invalid RuntimeEvent',
      )
    }
    const matches = [...this.#states.values()].filter(state => (
      state.binding.codexSessionId === event.source.kernelSessionId
      && (state.binding.dshSessionId === null
        || state.binding.dshSessionId === event.source.sessionId)
    ))
    if (matches.length === 0) {
      throw new DeliveryRuntimeProjectionError(
        'RUNTIME_SESSION_UNBOUND',
        `runtime event ${event.id} is not bound to Delivery ${this.delivery.id}`,
      )
    }
    if (matches.length > 1) {
      throw new DeliveryRuntimeProjectionError(
        'RUNTIME_SESSION_AMBIGUOUS',
        `runtime event ${event.id} matches more than one SessionBinding`,
      )
    }
    return matches[0]!
  }

  #applyPlan(state: MutableSessionState, event: RuntimeEvent): void {
    if (event.semantic?.kind !== 'plan') return
    const semantic = event.semantic
    const reference = eventLink(event)
    const existing = state.plan
    const text = semantic.mode === 'delta'
      ? `${existing?.text ?? ''}${semantic.text ?? ''}` || null
      : semantic.text ?? existing?.text ?? null
    state.plan = Object.freeze({
      itemId: semantic.itemId ?? existing?.itemId ?? null,
      explanation: semantic.explanation ?? existing?.explanation ?? null,
      items: semantic.items.length > 0
        ? Object.freeze(semantic.items.map(item => Object.freeze({ ...item })))
        : existing?.items ?? Object.freeze([]),
      text,
      complete: semantic.mode === 'completed',
      firstEvent: existing?.firstEvent ?? reference,
      latestEvent: reference,
    })
  }

  #applyAgents(state: MutableSessionState, event: RuntimeEvent): void {
    const reference = eventLink(event)
    const rootThreadId = state.binding.codexSessionId ?? event.source.kernelSessionId
    const existingRoot = state.agents.get(rootThreadId)
    const rootStatus: DeliveryRuntimeAgentStatus = event.kind === 'turn.started'
      ? 'running'
      : event.kind === 'turn.completed'
        ? event.terminalReason === 'failed' ? 'failed' : 'completed'
        : event.kind === 'turn.aborted' || event.kind === 'failure'
          ? 'failed'
          : existingRoot?.status ?? 'unknown'
    state.agents.set(rootThreadId, Object.freeze({
      threadId: rootThreadId,
      path: existingRoot?.path ?? '/root',
      parentThreadId: null,
      nickname: existingRoot?.nickname ?? null,
      role: existingRoot?.role ?? state.stageRun.role,
      status: rootStatus,
      firstEvent: existingRoot?.firstEvent ?? reference,
      latestEvent: reference,
    }))
    if (event.semantic?.kind !== 'agent-graph') return
    for (const change of event.semantic.changes) {
      this.#upsertAgent(state, change, reference, rootThreadId)
    }
  }

  #upsertAgent(
    state: MutableSessionState,
    change: RuntimeAgentGraphChange,
    reference: DeliveryRuntimeEventLink,
    rootThreadId: string,
  ): void {
    const existing = state.agents.get(change.threadId)
    state.agents.set(change.threadId, Object.freeze({
      threadId: change.threadId,
      path: change.path ?? existing?.path ?? null,
      parentThreadId: change.parentThreadId ?? existing?.parentThreadId ?? rootThreadId,
      nickname: change.nickname ?? existing?.nickname ?? null,
      role: change.role ?? existing?.role ?? null,
      status: graphStatus(change.status),
      firstEvent: existing?.firstEvent ?? reference,
      latestEvent: reference,
    }))
  }

  #applyActivity(state: MutableSessionState, event: RuntimeEvent): void {
    if (!event.kind.startsWith('tool.') || !isCommandEvent(event)) return
    const callId = event.source.toolCallId
      ?? nonEmptyString(event.data.call_id)
      ?? nonEmptyString(nestedRecord(event.data, 'item')?.id)
    if (callId === undefined) return
    const existing = state.activities.get(callId)
    const command = commandFrom(event) ?? existing?.command ?? null
    const reference = eventLink(event)
    const activityType = isTestCommand(command) ? 'test' as const : 'command' as const
    const activity = Object.freeze({
      callId,
      activityType,
      command,
      status: activityStatus(event, activityType),
      outcome: deliveryRuntimeEvidenceOutcome(event, activityType),
      exitCode: exitCode(event) ?? existing?.exitCode ?? null,
      firstEvent: existing?.firstEvent ?? reference,
      latestEvent: reference,
    })
    state.activities.set(callId, activity)
    if (event.kind === 'tool.completed') {
      this.#addEvidence(state, event, activity.activityType)
      this.#resolveInteractions(state, event, callId)
    }
  }

  #applyInteraction(state: MutableSessionState, event: RuntimeEvent): void {
    if (event.kind === 'approval.requested' || event.kind === 'input.requested') {
      const semantic = event.semantic?.kind === 'input' ? event.semantic : undefined
      const id = event.source.approvalId
        ?? semantic?.requestId
        ?? nonEmptyString(event.data.call_id)
        ?? event.id
      const operationId = nonEmptyString(event.data.call_id) ?? id
      state.interactions.set(id, Object.freeze({
        id,
        operationId,
        interactionType: event.kind === 'input.requested'
          ? 'user-input'
          : 'execution-approval',
        blocking: semantic?.blocking ?? true,
        status: 'pending',
        questions: semantic?.questions ?? Object.freeze([]),
        requestedEvent: eventLink(event),
        resolvedEvent: null,
      }))
      return
    }
    if (event.kind === 'tool.started' || event.kind === 'tool.completed') {
      const operationId = event.source.toolCallId ?? nonEmptyString(event.data.call_id)
      if (operationId !== undefined) this.#resolveInteractions(state, event, operationId)
    }
    if (event.kind === 'turn.completed' || event.kind === 'turn.aborted') {
      for (const interaction of state.interactions.values()) {
        if (interaction.status === 'pending') this.#resolveInteraction(state, interaction, event)
      }
    }
  }

  #resolveInteractions(state: MutableSessionState, event: RuntimeEvent, operationId: string): void {
    for (const interaction of state.interactions.values()) {
      if (interaction.status === 'pending' && interaction.operationId === operationId) {
        this.#resolveInteraction(state, interaction, event)
      }
    }
  }

  #resolveInteraction(
    state: MutableSessionState,
    interaction: DeliveryRuntimeInteraction,
    event: RuntimeEvent,
  ): void {
    state.interactions.set(interaction.id, Object.freeze({
      ...interaction,
      status: 'resolved',
      resolvedEvent: eventLink(event),
    }))
  }

  #applyFailureAndRecovery(state: MutableSessionState, event: RuntimeEvent): void {
    const reference = eventLink(event)
    if (isFailureEvent(event)) {
      state.failures.push(Object.freeze({
        message: failureMessage(event),
        code: failureCode(event),
        event: reference,
      }))
      state.recovery = Object.freeze({
        state: 'required',
        failureCount: state.recovery.failureCount + 1,
        recoveryCount: state.recovery.recoveryCount,
        lastFailureEvent: reference,
        latestRecoveryEvent: state.recovery.latestRecoveryEvent,
      })
      this.#addEvidence(state, event, 'runtime_event')
      return
    }
    if (state.recovery.state === 'required'
      && (event.kind === 'session.configured' || event.kind === 'turn.started')) {
      state.recovery = Object.freeze({
        ...state.recovery,
        state: 'in-progress',
        latestRecoveryEvent: reference,
      })
      return
    }
    if (state.recovery.state === 'in-progress'
      && event.kind === 'turn.completed'
      && event.terminalReason === 'completed') {
      state.recovery = Object.freeze({
        ...state.recovery,
        state: 'recovered',
        recoveryCount: state.recovery.recoveryCount + 1,
        latestRecoveryEvent: reference,
      })
    }
  }

  #addEvidence(
    state: MutableSessionState,
    event: RuntimeEvent,
    type: DeliveryRuntimeEvidenceLink['type'],
  ): void {
    const sourceRef = `runtime_event:${event.id}`
    state.evidence.set(`${sourceRef}\u0000${type}`, Object.freeze({
      type,
      outcome: deliveryRuntimeEvidenceOutcome(event, type),
      sourceRef,
      stageRunId: state.stageRun.id,
      sessionBindingId: state.binding.id,
      eventId: event.id,
    }))
  }

  #sessionSnapshot(state: MutableSessionState): DeliverySessionRuntimeView {
    const agents = [...state.agents.values()].sort((left, right) => (
      left.threadId.localeCompare(right.threadId)
    ))
    const agentEdges = uniqueBy(
      agents.flatMap(agent => agent.parentThreadId === null
        ? []
        : [{ parentThreadId: agent.parentThreadId, childThreadId: agent.threadId }]),
      edge => `${edge.parentThreadId}\u0000${edge.childThreadId}`,
    )
    const interactions = [...state.interactions.values()].sort((left, right) => (
      left.id.localeCompare(right.id)
    ))
    const evidenceLinks = [...state.evidence.values()].sort((left, right) => (
      left.sourceRef.localeCompare(right.sourceRef) || left.type.localeCompare(right.type)
    ))
    const attentionCandidates = interactions
      .filter(interaction => interaction.interactionType === 'user-input')
      .map(interaction => Object.freeze({
        type: attentionType(state.stageRun),
        title: attentionTitle(interaction.questions),
        blocking: interaction.blocking,
        status: interaction.status === 'pending' ? 'open' as const : 'resolved' as const,
        stageRunId: state.stageRun.id,
        sessionBindingId: state.binding.id,
        sourceRef: interaction.requestedEvent.sourceRef,
        questions: interaction.questions,
      }))
    return immutable({
      binding: state.binding,
      asOfSequence: state.sequence.toString(),
      plan: state.plan,
      agents,
      agentEdges,
      activities: [...state.activities.values()].sort((left, right) => (
        left.callId.localeCompare(right.callId)
      )),
      interactions,
      failures: state.failures,
      recovery: state.recovery,
      diff: state.diff,
      usage: state.usage,
      evidenceLinks,
      attentionCandidates,
    })
  }
}

export function projectDeliveryRuntime(
  delivery: Delivery,
  events: Iterable<RuntimeEvent>,
): DeliveryRuntimeProjectionSnapshot {
  return new DeliveryRuntimeProjection({ delivery }).replay(events)
}
