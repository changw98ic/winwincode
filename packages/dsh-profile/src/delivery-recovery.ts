import {
  DeliveryId,
  type Delivery,
  type DeliveryId as DeliveryIdentifier,
  type DeliveryStage,
  type DeliveryTask,
  type DeliveryTaskId,
  type FrozenDeliveryCandidate,
  type RuntimeEvent,
  type SessionBinding,
  type StageRun,
} from '@winwincode/contracts'
import {
  DeliveryRuntimeProjection,
  DeliveryStore,
  assertFrozenDeliveryCandidateCurrent,
  type DeliveryRuntimeProjectionSnapshot,
} from '@winwincode/strongflow'

import {
  DshRuntimeProjection,
  type DshRuntimeSnapshot,
} from './runtime-projection.js'
import {
  RuntimeSessionLedger,
  type RuntimeSessionManifest,
} from './session-ledger.js'

export const DELIVERY_RECOVERY_SCHEMA_VERSION = 1 as const

export type DeliveryRecoveryErrorCode =
  | 'INVALID_RECOVERY_OPTIONS'
  | 'DELIVERY_STORE_FAILURE'
  | 'DELIVERY_STATE_CONFLICT'
  | 'SESSION_BINDING_CONFLICT'
  | 'SESSION_LEDGER_CONFLICT'
  | 'CANDIDATE_CONFLICT'
  | 'CODEX_SESSION_QUERY_FAILED'

/** Visible restart failure; recovery never guesses past conflicting owner records. */
export class DeliveryRecoveryError extends Error {
  readonly code: DeliveryRecoveryErrorCode

  constructor(code: DeliveryRecoveryErrorCode, message: string, options?: ErrorOptions) {
    super(message, options)
    this.name = 'DeliveryRecoveryError'
    this.code = code
  }
}

/** The only Codex query used during reconciliation. It does not execute or resume a thread. */
export interface DeliveryRecoveryCodexPort {
  listSessions(): Promise<readonly string[]>
}

export interface ReconcileDeliveryAfterRestartOptions {
  readonly home: string
  readonly deliveryId: DeliveryIdentifier | string
  readonly codex: DeliveryRecoveryCodexPort
  /** Rebuilt Git fact, when the caller already has one. It is never persisted here. */
  readonly candidate?: FrozenDeliveryCandidate | null
}

export interface DeliveryRecoverySessionView {
  readonly binding: SessionBinding
  readonly manifest: RuntimeSessionManifest
  readonly runtimeRecordCount: number
  readonly runtimeEventCount: number
  readonly dsh: DshRuntimeSnapshot
}

interface ActiveSessionActionFields {
  readonly stageRunId: string
  readonly sessionBindingId: string
  readonly dshSessionId: string
  readonly codexSessionId: string
}

export type DeliveryRecoveryAction =
  | {
      readonly kind: 'resolve-delivery-attention'
      readonly attentionItemIds: readonly string[]
    }
  | {
      readonly kind: 'create-stage-session'
      readonly stageRunId: string
      readonly stage: DeliveryStage
      readonly actorType: StageRun['actorType']
      readonly role: string
    }
  | ({
      readonly kind: 'resume-stage-session'
      readonly rolloutPath: string
      readonly pendingInteractionIds: readonly string[]
    } & ActiveSessionActionFields)
  | ({
      readonly kind: 'resolve-runtime-interaction'
      readonly interactionIds: readonly string[]
    } & ActiveSessionActionFields)
  | ({
      readonly kind: 'review-stage-output'
      readonly runtimeStatus: 'completed' | 'aborted' | 'failed'
    } & ActiveSessionActionFields)
  | ({ readonly kind: 'continue-stage' } & ActiveSessionActionFields)
  | {
      readonly kind: 'start-stage'
      readonly stage: DeliveryStage
      readonly deliveryTaskId: DeliveryTaskId | null
    }
  | { readonly kind: 'delivery-complete' }

export interface DeliveryRecoverySnapshot {
  readonly schemaVersion: typeof DELIVERY_RECOVERY_SCHEMA_VERSION
  readonly delivery: Delivery
  readonly deliveryRecordSequence: string
  readonly candidateRef: string | null
  readonly verdictCandidateRef: string | null
  readonly liveBoundCodexSessionIds: readonly string[]
  readonly sessions: readonly DeliveryRecoverySessionView[]
  readonly strongFlow: DeliveryRuntimeProjectionSnapshot
  readonly nextAction: DeliveryRecoveryAction
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
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

function recoveryError(
  code: DeliveryRecoveryErrorCode,
  message: string,
  cause?: unknown,
): never {
  throw new DeliveryRecoveryError(
    code,
    message,
    cause === undefined ? undefined : { cause },
  )
}

function activeStageRuns(delivery: Delivery): readonly StageRun[] {
  return delivery.stageRuns.filter(run => run.status === 'running' || run.status === 'waiting')
}

function bindingsByRun(delivery: Delivery): ReadonlyMap<string, readonly SessionBinding[]> {
  const grouped = new Map<string, SessionBinding[]>()
  for (const binding of delivery.sessionBindings) {
    const bindings = grouped.get(binding.stageRunId) ?? []
    bindings.push(binding)
    grouped.set(binding.stageRunId, bindings)
  }
  return grouped
}

function validateBindingShape(
  delivery: Delivery,
): ReadonlyMap<string, readonly SessionBinding[]> {
  const grouped = bindingsByRun(delivery)
  const dshOwners = new Map<string, string>()
  const codexOwners = new Map<string, string>()
  const runs = new Map(delivery.stageRuns.map(run => [run.id, run]))
  for (const binding of delivery.sessionBindings) {
    if (binding.dshSessionId !== null
      && runs.get(binding.stageRunId)?.actorType === 'codex') {
      const owner = dshOwners.get(binding.dshSessionId)
      if (owner !== undefined) {
        recoveryError(
          'SESSION_BINDING_CONFLICT',
          `DSH session ${binding.dshSessionId} is bound by both ${owner} and ${binding.id}`,
        )
      }
      dshOwners.set(binding.dshSessionId, binding.id)
    }
    if (binding.codexSessionId !== null) {
      const owner = codexOwners.get(binding.codexSessionId)
      if (owner !== undefined) {
        recoveryError(
          'SESSION_BINDING_CONFLICT',
          `Codex session ${binding.codexSessionId} is bound by both ${owner} and ${binding.id}`,
        )
      }
      codexOwners.set(binding.codexSessionId, binding.id)
    }
  }

  for (const run of delivery.stageRuns) {
    const bindings = grouped.get(run.id) ?? []
    const active = run.status === 'running' || run.status === 'waiting'
    if ((!active && bindings.length !== 1) || (active && bindings.length > 1)) {
      recoveryError(
        'SESSION_BINDING_CONFLICT',
        `StageRun ${run.id} has ${String(bindings.length)} owning SessionBindings`,
      )
    }
    const binding = bindings[0]
    if (binding === undefined) continue
    if (run.actorType === 'codex'
      && (binding.dshSessionId === null || binding.codexSessionId === null)) {
      recoveryError(
        'SESSION_BINDING_CONFLICT',
        `Codex StageRun ${run.id} requires one complete DSH and Codex SessionBinding`,
      )
    }
    if (run.actorType === 'human'
      && (binding.dshSessionId === null || binding.codexSessionId !== null)) {
      recoveryError(
        'SESSION_BINDING_CONFLICT',
        `human StageRun ${run.id} requires one DSH-only SessionBinding`,
      )
    }
  }
  return grouped
}

function validateActiveStage(delivery: Delivery, active: readonly StageRun[]): void {
  if (active.length > 1) {
    recoveryError('DELIVERY_STATE_CONFLICT', 'Delivery has more than one active StageRun')
  }
  const run = active[0]
  if (run === undefined) return
  const stageMatches = run.stage === delivery.status
    || (delivery.status === 'needs-attention'
      && (run.stage === 'plan-review' || run.stage === 'delivery-review'))
  if (!stageMatches) {
    recoveryError(
      'DELIVERY_STATE_CONFLICT',
      `active StageRun ${run.id} does not match Delivery status ${delivery.status}`,
    )
  }
}

function validateCandidateVerdictIdentity(
  delivery: Delivery,
  candidate: FrozenDeliveryCandidate,
): void {
  const verdict = delivery.verdict
  if (verdict === null || verdict.candidateRef === candidate.candidateRef) return
  const producer = delivery.stageRuns.find(run => run.id === candidate.producerStageRunId)
  const supersedesVerdict = producer?.stage === 'reworking'
    && producer.startedAtMillis >= verdict.producedAtMillis
  if (!supersedesVerdict) {
    recoveryError(
      'CANDIDATE_CONFLICT',
      `Delivery ${delivery.id} candidate does not match its current DeliveryVerdict`,
    )
  }
}

function runnableTask(
  delivery: Delivery,
  acceptedStatuses: readonly DeliveryTask['status'][],
): DeliveryTaskId | null {
  if (delivery.tasks.length === 0) return null
  const tasks = new Map(delivery.tasks.map(task => [task.id, task]))
  const task = delivery.tasks.find(entry => (
    acceptedStatuses.includes(entry.status)
    && entry.blockedByTaskIds.every(dependencyId => tasks.get(dependencyId)?.status === 'completed')
  ))
  if (task === undefined) {
    recoveryError(
      'DELIVERY_STATE_CONFLICT',
      `Delivery ${delivery.id} has no runnable task for ${delivery.status}`,
    )
  }
  return task.id
}

function startAction(delivery: Delivery): DeliveryRecoveryAction {
  switch (delivery.status) {
    case 'draft':
    case 'clarifying':
      return Object.freeze({ kind: 'start-stage', stage: 'clarifying', deliveryTaskId: null })
    case 'ready':
    case 'planning':
      return Object.freeze({ kind: 'start-stage', stage: 'planning', deliveryTaskId: null })
    case 'executing':
      return Object.freeze({
        kind: 'start-stage',
        stage: 'executing',
        deliveryTaskId: runnableTask(delivery, ['pending']),
      })
    case 'verifying':
      return Object.freeze({
        kind: 'start-stage',
        stage: 'verifying',
        deliveryTaskId: runnableTask(delivery, ['verifying']),
      })
    case 'reworking':
      return Object.freeze({
        kind: 'start-stage',
        stage: 'reworking',
        deliveryTaskId: runnableTask(delivery, ['failed']),
      })
    case 'ready-to-deliver':
      return Object.freeze({
        kind: 'start-stage',
        stage: 'delivery-review',
        deliveryTaskId: null,
      })
    case 'delivered':
      return Object.freeze({ kind: 'delivery-complete' })
    case 'needs-attention':
      return recoveryError(
        'DELIVERY_STATE_CONFLICT',
        'needs-attention Delivery has no unresolved blocking AttentionItem',
      )
    case 'plan-review':
      return recoveryError(
        'DELIVERY_STATE_CONFLICT',
        'plan-review is not a canonical stored Delivery status',
      )
  }
}

function activeSessionFields(run: StageRun, binding: SessionBinding): ActiveSessionActionFields {
  if (binding.dshSessionId === null || binding.codexSessionId === null) {
    return recoveryError(
      'SESSION_BINDING_CONFLICT',
      `active Codex StageRun ${run.id} has an incomplete SessionBinding`,
    )
  }
  return Object.freeze({
    stageRunId: run.id,
    sessionBindingId: binding.id,
    dshSessionId: binding.dshSessionId,
    codexSessionId: binding.codexSessionId,
  })
}

function selectNextAction(
  delivery: Delivery,
  active: readonly StageRun[],
  groupedBindings: ReadonlyMap<string, readonly SessionBinding[]>,
  sessionsByBinding: ReadonlyMap<string, DeliveryRecoverySessionView>,
  liveSessions: ReadonlySet<string>,
): DeliveryRecoveryAction {
  const run = active[0]
  const activeBindings = run === undefined ? [] : (groupedBindings.get(run.id) ?? [])
  if (run?.actorType === 'human' && activeBindings.length === 0) {
    return Object.freeze({
      kind: 'create-stage-session',
      stageRunId: run.id,
      stage: run.stage,
      actorType: run.actorType,
      role: run.role,
    })
  }
  const blockingAttention = delivery.attentionItems.filter(item => (
    item.blocking && item.status === 'open'
  ))
  if (blockingAttention.length > 0) {
    if (delivery.status !== 'needs-attention') {
      return recoveryError(
        'DELIVERY_STATE_CONFLICT',
        'open blocking Attention exists outside needs-attention state',
      )
    }
    return Object.freeze({
      kind: 'resolve-delivery-attention',
      attentionItemIds: Object.freeze(blockingAttention.map(item => item.id)),
    })
  }

  if (run === undefined) return startAction(delivery)
  if (run.actorType === 'human') {
    return recoveryError(
      'DELIVERY_STATE_CONFLICT',
      `active human StageRun ${run.id} has no unresolved blocking AttentionItem`,
    )
  }
  const binding = (groupedBindings.get(run.id) ?? [])[0]
  if (binding === undefined) {
    return Object.freeze({
      kind: 'create-stage-session',
      stageRunId: run.id,
      stage: run.stage,
      actorType: run.actorType,
      role: run.role,
    })
  }
  const fields = activeSessionFields(run, binding)
  const session = sessionsByBinding.get(binding.id)
  if (session === undefined) {
    return recoveryError(
      'SESSION_LEDGER_CONFLICT',
      `active StageRun ${run.id} has no rebuilt runtime session`,
    )
  }
  const pendingInteractionIds = session.dsh.pendingApprovals.map(approval => approval.id)
  const live = liveSessions.has(fields.codexSessionId)
  if (pendingInteractionIds.length > 0) {
    if (!live) {
      return Object.freeze({
        kind: 'resume-stage-session',
        ...fields,
        rolloutPath: session.manifest.rolloutPath,
        pendingInteractionIds: Object.freeze(pendingInteractionIds),
      })
    }
    return Object.freeze({
      kind: 'resolve-runtime-interaction',
      ...fields,
      interactionIds: Object.freeze(pendingInteractionIds),
    })
  }
  if (session.dsh.status === 'awaiting_approval') {
    return recoveryError(
      'SESSION_LEDGER_CONFLICT',
      `runtime session ${fields.dshSessionId} is awaiting an interaction that is absent`,
    )
  }
  if (session.dsh.status === 'completed'
    || session.dsh.status === 'aborted'
    || session.dsh.status === 'failed') {
    return Object.freeze({
      kind: 'review-stage-output',
      ...fields,
      runtimeStatus: session.dsh.status,
    })
  }
  if (!live) {
    return Object.freeze({
      kind: 'resume-stage-session',
      ...fields,
      rolloutPath: session.manifest.rolloutPath,
      pendingInteractionIds: Object.freeze([]),
    })
  }
  return Object.freeze({ kind: 'continue-stage', ...fields })
}

function validateSettledRuntimeInteractions(
  delivery: Delivery,
  sessionsByBinding: ReadonlyMap<string, DeliveryRecoverySessionView>,
): void {
  const runs = new Map(delivery.stageRuns.map(run => [run.id, run]))
  for (const binding of delivery.sessionBindings) {
    const run = runs.get(binding.stageRunId)!
    const session = sessionsByBinding.get(binding.id)
    if ((run.status !== 'running' && run.status !== 'waiting')
      && session !== undefined
      && session.dsh.pendingApprovals.length > 0) {
      recoveryError(
        'DELIVERY_STATE_CONFLICT',
        `settled StageRun ${run.id} retains unresolved runtime interaction`,
      )
    }
  }
}

/**
 * Rebuild Delivery and UI projections after restart. The result contains one
 * deterministic delivery-level action but never executes, resumes, or replays
 * a Codex operation.
 */
export async function reconcileDeliveryAfterRestart(
  options: ReconcileDeliveryAfterRestartOptions,
): Promise<DeliveryRecoverySnapshot> {
  if (!isRecord(options)
    || typeof options.home !== 'string'
    || options.home.length === 0
    || typeof options.deliveryId !== 'string'
    || typeof options.codex?.listSessions !== 'function') {
    return recoveryError(
      'INVALID_RECOVERY_OPTIONS',
      'delivery recovery requires a home, Delivery id, and Codex session query',
    )
  }
  let deliveryId: DeliveryIdentifier
  try {
    deliveryId = DeliveryId(options.deliveryId)
  } catch (error) {
    return recoveryError('INVALID_RECOVERY_OPTIONS', 'delivery recovery id is invalid', error)
  }

  let stored: Awaited<ReturnType<DeliveryStore['read']>>
  try {
    const store = await DeliveryStore.open(options.home, deliveryId)
    stored = await store.read()
  } catch (error) {
    return recoveryError(
      'DELIVERY_STORE_FAILURE',
      `Delivery ${deliveryId} could not be rebuilt from its owner records`,
      error,
    )
  }
  const delivery = stored.snapshot
  const groupedBindings = validateBindingShape(delivery)
  const active = activeStageRuns(delivery)
  validateActiveStage(delivery, active)

  let candidate: FrozenDeliveryCandidate | null = null
  if (options.candidate !== undefined && options.candidate !== null) {
    try {
      candidate = assertFrozenDeliveryCandidateCurrent(delivery, options.candidate)
      validateCandidateVerdictIdentity(delivery, candidate)
    } catch (error) {
      return recoveryError(
        'CANDIDATE_CONFLICT',
        `Delivery ${delivery.id} candidate is stale or belongs to another owner record`,
        error,
      )
    }
  }

  const runs = new Map(delivery.stageRuns.map(run => [run.id, run]))
  const sessionViews: DeliveryRecoverySessionView[] = []
  const events: RuntimeEvent[] = []
  for (const binding of delivery.sessionBindings) {
    const run = runs.get(binding.stageRunId)!
    if (run.actorType !== 'codex') continue
    if (binding.dshSessionId === null || binding.codexSessionId === null) {
      return recoveryError(
        'SESSION_BINDING_CONFLICT',
        `Codex StageRun ${run.id} has an incomplete SessionBinding`,
      )
    }
    try {
      const ledger = await RuntimeSessionLedger.open(options.home, binding.dshSessionId)
      const ledgerSnapshot = await ledger.read()
      const manifest = ledgerSnapshot.manifest
      if (manifest.roleId !== run.role
        || manifest.kernelSessionId !== binding.codexSessionId) {
        return recoveryError(
          'SESSION_LEDGER_CONFLICT',
          `runtime ledger ${binding.dshSessionId} conflicts with SessionBinding ${binding.id}`,
        )
      }
      const projection = new DshRuntimeProjection({
        sessionId: binding.dshSessionId,
        roleId: run.role,
        provider: manifest.provider,
        model: manifest.model,
      })
      projection.replay(ledgerSnapshot.events)
      sessionViews.push(Object.freeze({
        binding,
        manifest,
        runtimeRecordCount: ledgerSnapshot.records.length,
        runtimeEventCount: ledgerSnapshot.events.length,
        dsh: projection.snapshot,
      }))
      events.push(...ledgerSnapshot.events)
    } catch (error) {
      if (error instanceof DeliveryRecoveryError) throw error
      return recoveryError(
        'SESSION_LEDGER_CONFLICT',
        `runtime ledger for SessionBinding ${binding.id} could not be rebuilt`,
        error,
      )
    }
  }

  let strongFlow: DeliveryRuntimeProjectionSnapshot
  try {
    strongFlow = new DeliveryRuntimeProjection({ delivery }).replay(events)
  } catch (error) {
    return recoveryError(
      'SESSION_LEDGER_CONFLICT',
      `StrongFlow runtime view for Delivery ${delivery.id} could not be rebuilt`,
      error,
    )
  }
  const sessionsByBinding = new Map(sessionViews.map(view => [view.binding.id, view]))
  validateSettledRuntimeInteractions(delivery, sessionsByBinding)

  let queriedSessions: readonly string[]
  try {
    queriedSessions = await options.codex.listSessions()
  } catch (error) {
    return recoveryError(
      'CODEX_SESSION_QUERY_FAILED',
      'Codex session state could not be queried through its public boundary',
      error,
    )
  }
  if (!Array.isArray(queriedSessions)
    || queriedSessions.some(sessionId => typeof sessionId !== 'string' || sessionId.length === 0)
    || new Set(queriedSessions).size !== queriedSessions.length) {
    return recoveryError(
      'CODEX_SESSION_QUERY_FAILED',
      'Codex returned an invalid session list',
    )
  }
  const liveSessions = new Set(queriedSessions)
  const boundCodexSessions = new Set(delivery.sessionBindings.flatMap(binding => (
    binding.codexSessionId === null ? [] : [binding.codexSessionId]
  )))
  const liveBoundCodexSessionIds = [...liveSessions]
    .filter(sessionId => boundCodexSessions.has(sessionId))
    .sort()
  const nextAction = selectNextAction(
    delivery,
    active,
    groupedBindings,
    sessionsByBinding,
    liveSessions,
  )

  return immutable({
    schemaVersion: DELIVERY_RECOVERY_SCHEMA_VERSION,
    delivery,
    deliveryRecordSequence: stored.records.at(-1)!.sequence,
    candidateRef: candidate?.candidateRef ?? null,
    verdictCandidateRef: delivery.verdict?.candidateRef ?? null,
    liveBoundCodexSessionIds,
    sessions: sessionViews,
    strongFlow,
    nextAction,
  })
}
