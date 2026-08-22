import { randomUUID } from 'node:crypto'

import {
  AttemptId,
  KernelSessionId,
  StageRunId,
  STRONGFLOW_JOB_STAGES,
  STRONGFLOW_STAGE_BY_STATE,
  createStrongFlowJobEvent,
  type AttemptId as AttemptIdentifier,
  type CandidateId,
  type KernelSessionId as KernelSessionIdentifier,
  type StageFailureCategory,
  type StageRunId as StageRunIdentifier,
  type StageSucceededData,
  type StrongFlowJobSnapshot,
  type StrongFlowJobStage,
} from '@winwincode/contracts'

import {
  StrongFlowJobStore,
  StrongFlowJobStoreError,
} from './job-store.js'

export type StrongFlowControllerOperation = StrongFlowJobStage | 'COMPLETION_GATE'

export type StrongFlowStageOutput<Stage extends StrongFlowJobStage> = Omit<
  Extract<StageSucceededData, { readonly stage: Stage }>,
  'stage' | 'stageRunId' | 'attemptId'
>

export interface StrongFlowStageContext<Stage extends StrongFlowJobStage> {
  readonly stage: Stage
  readonly stageRunId: StageRunIdentifier
  readonly attemptId: AttemptIdentifier
  readonly snapshot: StrongFlowJobSnapshot
  readonly signal: AbortSignal
}

export interface StrongFlowStageRunResult<Stage extends StrongFlowJobStage> {
  readonly output: StrongFlowStageOutput<Stage>
  readonly kernelSessionId?: KernelSessionIdentifier
}

export interface StrongFlowStageProvider<Stage extends StrongFlowJobStage> {
  readonly stage: Stage
  readonly roleId: string
  run(context: StrongFlowStageContext<Stage>): Promise<StrongFlowStageRunResult<Stage>>
}

export type AnyStrongFlowStageProvider = {
  readonly [Stage in StrongFlowJobStage]: StrongFlowStageProvider<Stage>
}[StrongFlowJobStage]

export interface StrongFlowCompletionGateContext {
  readonly stageRunId: StageRunIdentifier
  readonly candidateId: CandidateId
  readonly snapshot: StrongFlowJobSnapshot
  readonly signal: AbortSignal
}

export type StrongFlowCompletionGateResult =
  | { readonly outcome: 'passed' }
  | { readonly outcome: 'failed'; readonly reason: string }

/** Program-owned gate. Model roles are stage providers and cannot implement workflow decisions. */
export interface StrongFlowCompletionGate {
  readonly authority: 'program'
  evaluate(context: StrongFlowCompletionGateContext): Promise<StrongFlowCompletionGateResult>
}

export type StrongFlowStageProviderFailureOptions = {
  readonly category: StageFailureCategory
  readonly code: string
  readonly message: string
  readonly retryable: boolean
  readonly kernelSessionId?: KernelSessionIdentifier
  readonly cause?: unknown
}

/** A provider may expose only a pre-sanitized failure record to durable job history. */
export class StrongFlowStageProviderFailure extends Error {
  readonly category: StageFailureCategory
  readonly code: string
  readonly retryable: boolean
  readonly kernelSessionId?: KernelSessionIdentifier

  constructor(options: StrongFlowStageProviderFailureOptions) {
    if (
      !['task', 'infrastructure'].includes(options.category)
      || typeof options.code !== 'string'
      || options.code.trim().length === 0
      || typeof options.message !== 'string'
      || options.message.trim().length === 0
      || typeof options.retryable !== 'boolean'
    ) throw new TypeError('StrongFlow stage provider failure is invalid')
    super(options.message, options.cause === undefined ? undefined : { cause: options.cause })
    this.name = 'StrongFlowStageProviderFailure'
    this.category = options.category
    this.code = options.code
    this.retryable = options.retryable
    if (options.kernelSessionId !== undefined) this.kernelSessionId = options.kernelSessionId
  }
}

export type StrongFlowControllerErrorCode =
  | 'INVALID_CONTROLLER_OPTIONS'
  | 'MISSING_STAGE_PROVIDER'
  | 'DUPLICATE_STAGE_PROVIDER'
  | 'CONTROLLER_CONFLICT'
  | 'INVALID_STAGE_RESULT'
  | 'STEP_LIMIT_REACHED'
  | 'JOB_NOT_INTERRUPTED'
  | 'JOB_TERMINAL'

export class StrongFlowControllerError extends Error {
  readonly code: StrongFlowControllerErrorCode

  constructor(code: StrongFlowControllerErrorCode, message: string, options?: ErrorOptions) {
    super(message, options)
    this.name = 'StrongFlowControllerError'
    this.code = code
  }
}

export interface StrongFlowControllerOptions {
  readonly store: StrongFlowJobStore
  readonly providers: readonly AnyStrongFlowStageProvider[]
  readonly completionGate: StrongFlowCompletionGate
  readonly controllerId?: string
  readonly clock?: () => number
  readonly stageRunIdFactory?: (
    operation: StrongFlowControllerOperation,
    snapshot: StrongFlowJobSnapshot,
  ) => StageRunIdentifier
  readonly attemptIdFactory?: (
    stage: StrongFlowJobStage,
    snapshot: StrongFlowJobSnapshot,
  ) => AttemptIdentifier
  readonly maxAutomaticTransitions?: number
}

export type StrongFlowControllerAdvanceResult =
  | {
    readonly kind: 'stage-succeeded'
    readonly stage: StrongFlowJobStage
    readonly snapshot: StrongFlowJobSnapshot
  }
  | {
    readonly kind: 'stage-failed'
    readonly stage: StrongFlowJobStage
    readonly snapshot: StrongFlowJobSnapshot
  }
  | {
    readonly kind: 'completion-gate-passed' | 'completion-gate-failed'
    readonly snapshot: StrongFlowJobSnapshot
  }
  | {
    readonly kind: 'delivered'
    readonly snapshot: StrongFlowJobSnapshot
  }
  | {
    readonly kind: 'waiting-for-human-review'
    readonly snapshot: StrongFlowJobSnapshot
  }
  | {
    readonly kind: 'active-stage'
    readonly snapshot: StrongFlowJobSnapshot
  }
  | {
    readonly kind: 'interrupted'
    readonly snapshot: StrongFlowJobSnapshot
  }
  | {
    readonly kind: 'terminal'
    readonly snapshot: StrongFlowJobSnapshot
  }

export interface StrongFlowControllerRunResult {
  readonly transitions: number
  readonly result: StrongFlowControllerAdvanceResult
}

export interface StrongFlowControllerRunOptions {
  readonly signal?: AbortSignal
  readonly maxTransitions?: number
}

const TERMINAL_STATES = new Set(['FAILED', 'REJECTED', 'CANCELLED', 'DELIVERED'])
const TRANSITION_RESULTS = new Set([
  'stage-succeeded',
  'completion-gate-passed',
  'completion-gate-failed',
])

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

function assertActorId(value: string, label: string): void {
  if (
    typeof value !== 'string'
    || value.length === 0
    || value.length > 200
    || value.trim() !== value
    || /[\u0000-\u001f\u007f]/u.test(value)
  ) {
    throw new StrongFlowControllerError(
      'INVALID_CONTROLLER_OPTIONS',
      `${label} is not a valid actor identity`,
    )
  }
}

interface RawStageResult {
  readonly output: Record<string, unknown>
  readonly kernelSessionId?: string
}

function assertExactResult(value: unknown): asserts value is RawStageResult {
  if (!isRecord(value)) {
    throw new StrongFlowControllerError(
      'INVALID_STAGE_RESULT',
      'stage provider result must be an object',
    )
  }
  const keys = Object.keys(value)
  if (
    !Object.hasOwn(value, 'output')
    || keys.some(key => !['output', 'kernelSessionId'].includes(key))
  ) {
    throw new StrongFlowControllerError(
      'INVALID_STAGE_RESULT',
      'stage provider result has an unexpected shape',
    )
  }
  if (!isRecord(value.output)) {
    throw new StrongFlowControllerError(
      'INVALID_STAGE_RESULT',
      'stage provider output must be an object',
    )
  }
  if (
    value.kernelSessionId !== undefined
    && typeof value.kernelSessionId !== 'string'
  ) {
    throw new StrongFlowControllerError(
      'INVALID_STAGE_RESULT',
      'stage provider kernelSessionId must be a string',
    )
  }
}

interface CombinedAbortSignal {
  readonly signal: AbortSignal
  dispose(): void
}

function combineSignals(internal: AbortSignal, external?: AbortSignal): CombinedAbortSignal {
  if (external === undefined) return { signal: internal, dispose() {} }
  const controller = new AbortController()
  const abort = (): void => controller.abort()
  internal.addEventListener('abort', abort, { once: true })
  external.addEventListener('abort', abort, { once: true })
  if (internal.aborted || external.aborted) abort()
  return {
    signal: controller.signal,
    dispose() {
      internal.removeEventListener('abort', abort)
      external.removeEventListener('abort', abort)
    },
  }
}

function nonEmptyReason(value: string, label: string): string {
  if (typeof value !== 'string' || value.trim().length === 0) {
    throw new StrongFlowControllerError(
      'INVALID_CONTROLLER_OPTIONS',
      `${label} must be a non-empty string`,
    )
  }
  return value
}

/** Drives StrongFlow only from persisted state and typed program ports. */
export class StrongFlowController {
  readonly store: StrongFlowJobStore
  readonly controllerId: string
  readonly #providers = new Map<StrongFlowJobStage, AnyStrongFlowStageProvider>()
  readonly #completionGate: StrongFlowCompletionGate
  readonly #clock: () => number
  readonly #stageRunIdFactory: NonNullable<StrongFlowControllerOptions['stageRunIdFactory']>
  readonly #attemptIdFactory: NonNullable<StrongFlowControllerOptions['attemptIdFactory']>
  readonly #maxAutomaticTransitions: number
  #tail: Promise<void> = Promise.resolve()
  #activeAbortController: AbortController | undefined
  #cancelRequested: string | undefined

  constructor(options: StrongFlowControllerOptions) {
    if (!isRecord(options)) {
      throw new StrongFlowControllerError(
        'INVALID_CONTROLLER_OPTIONS',
        'StrongFlow controller options must be an object',
      )
    }
    if (!(options.store instanceof StrongFlowJobStore)) {
      throw new StrongFlowControllerError(
        'INVALID_CONTROLLER_OPTIONS',
        'StrongFlow controller requires a job store',
      )
    }
    if (
      options.completionGate?.authority !== 'program'
      || typeof options.completionGate.evaluate !== 'function'
    ) {
      throw new StrongFlowControllerError(
        'INVALID_CONTROLLER_OPTIONS',
        'StrongFlow controller requires a program-owned completion gate',
      )
    }
    if (!Array.isArray(options.providers)) {
      throw new StrongFlowControllerError(
        'INVALID_CONTROLLER_OPTIONS',
        'StrongFlow controller providers must be an array',
      )
    }
    this.store = options.store
    this.controllerId = options.controllerId ?? 'strongflow-controller'
    assertActorId(this.controllerId, 'controllerId')
    for (const value of options.providers as readonly unknown[]) {
      if (!isRecord(value)) {
        throw new StrongFlowControllerError(
          'INVALID_CONTROLLER_OPTIONS',
          'stage provider must be an object',
        )
      }
      const provider = value as unknown as AnyStrongFlowStageProvider
      if (!STRONGFLOW_JOB_STAGES.includes(provider.stage)) {
        throw new StrongFlowControllerError(
          'INVALID_CONTROLLER_OPTIONS',
          'stage provider declares an unknown stage',
        )
      }
      assertActorId(provider.roleId, `provider ${provider.stage} roleId`)
      if (typeof provider.run !== 'function') {
        throw new StrongFlowControllerError(
          'INVALID_CONTROLLER_OPTIONS',
          `provider ${provider.stage} has no run function`,
        )
      }
      if (this.#providers.has(provider.stage)) {
        throw new StrongFlowControllerError(
          'DUPLICATE_STAGE_PROVIDER',
          `stage ${provider.stage} has more than one provider`,
        )
      }
      this.#providers.set(provider.stage, provider)
    }
    for (const stage of STRONGFLOW_JOB_STAGES) {
      if (!this.#providers.has(stage)) {
        throw new StrongFlowControllerError(
          'MISSING_STAGE_PROVIDER',
          `stage ${stage} has no provider`,
        )
      }
    }
    this.#completionGate = options.completionGate
    if (options.clock !== undefined && typeof options.clock !== 'function') {
      throw new StrongFlowControllerError(
        'INVALID_CONTROLLER_OPTIONS',
        'controller clock must be a function',
      )
    }
    if (
      options.stageRunIdFactory !== undefined
      && typeof options.stageRunIdFactory !== 'function'
    ) {
      throw new StrongFlowControllerError(
        'INVALID_CONTROLLER_OPTIONS',
        'stageRunIdFactory must be a function',
      )
    }
    if (
      options.attemptIdFactory !== undefined
      && typeof options.attemptIdFactory !== 'function'
    ) {
      throw new StrongFlowControllerError(
        'INVALID_CONTROLLER_OPTIONS',
        'attemptIdFactory must be a function',
      )
    }
    this.#clock = options.clock ?? Date.now
    this.#stageRunIdFactory = options.stageRunIdFactory
      ?? (operation => StageRunId(`run-${operation.toLowerCase()}-${randomUUID()}`))
    this.#attemptIdFactory = options.attemptIdFactory
      ?? (stage => AttemptId(`attempt-${stage.toLowerCase()}-${randomUUID()}`))
    this.#maxAutomaticTransitions = options.maxAutomaticTransitions ?? 64
    if (
      !Number.isSafeInteger(this.#maxAutomaticTransitions)
      || this.#maxAutomaticTransitions < 1
      || this.#maxAutomaticTransitions > 10_000
    ) {
      throw new StrongFlowControllerError(
        'INVALID_CONTROLLER_OPTIONS',
        'maxAutomaticTransitions must be an integer from 1 through 10000',
      )
    }
  }

  async advance(options: { readonly signal?: AbortSignal } = {}): Promise<
    StrongFlowControllerAdvanceResult
  > {
    return this.#serialize(async () => {
      const snapshot = (await this.store.read()).snapshot
      if (TERMINAL_STATES.has(snapshot.state)) {
        return Object.freeze({ kind: 'terminal', snapshot })
      }
      if (this.#cancelRequested !== undefined) {
        const cancelled = await this.#cancelSnapshot(snapshot, this.#cancelRequested)
        return Object.freeze({ kind: 'terminal', snapshot: cancelled })
      }
      if (snapshot.state === 'INTERRUPTED') {
        return Object.freeze({ kind: 'interrupted', snapshot })
      }
      if (snapshot.activeStage !== undefined) {
        return Object.freeze({ kind: 'active-stage', snapshot })
      }
      if (snapshot.state === 'AWAITING_HUMAN_REVIEW') {
        return Object.freeze({ kind: 'waiting-for-human-review', snapshot })
      }
      if (options.signal?.aborted === true) {
        const interrupted = await this.#interrupt(snapshot, 'controller run was interrupted')
        return Object.freeze({ kind: 'interrupted', snapshot: interrupted })
      }
      if (snapshot.state === 'AWAITING_COMPLETION_GATE') {
        return this.#runCompletionGate(snapshot, options.signal)
      }
      if (snapshot.state === 'READY_TO_DELIVER') return this.#recordDelivery(snapshot)
      const stage = STRONGFLOW_STAGE_BY_STATE[snapshot.state]
      if (stage === undefined) {
        throw new StrongFlowControllerError(
          'INVALID_CONTROLLER_OPTIONS',
          `controller has no deterministic action for state ${snapshot.state}`,
        )
      }
      return this.#runStage(stage, snapshot, options.signal)
    })
  }

  async runUntilPause(
    options: StrongFlowControllerRunOptions = {},
  ): Promise<StrongFlowControllerRunResult> {
    const limit = options.maxTransitions ?? this.#maxAutomaticTransitions
    if (!Number.isSafeInteger(limit) || limit < 1 || limit > 10_000) {
      throw new StrongFlowControllerError(
        'INVALID_CONTROLLER_OPTIONS',
        'run maxTransitions must be an integer from 1 through 10000',
      )
    }
    let transitions = 0
    while (transitions < limit) {
      const result = await this.advance(
        options.signal === undefined ? {} : { signal: options.signal },
      )
      if (TRANSITION_RESULTS.has(result.kind)) {
        transitions += 1
        if (transitions === limit) {
          const passive = this.#passiveResult(result.snapshot)
          if (passive !== undefined) return Object.freeze({ transitions, result: passive })
        }
        continue
      }
      if (result.kind === 'delivered' || result.kind === 'stage-failed') transitions += 1
      return Object.freeze({ transitions, result })
    }
    throw new StrongFlowControllerError(
      'STEP_LIMIT_REACHED',
      `controller reached its ${limit} transition limit`,
    )
  }

  async resume(): Promise<StrongFlowJobSnapshot> {
    return this.#serialize(async () => {
      const snapshot = (await this.store.read()).snapshot
      if (snapshot.state !== 'INTERRUPTED' || snapshot.interruption === undefined) {
        throw new StrongFlowControllerError(
          'JOB_NOT_INTERRUPTED',
          'only an interrupted StrongFlow job can resume',
        )
      }
      const event = createStrongFlowJobEvent({
        jobId: snapshot.jobId,
        sequence: this.#nextSequence(snapshot),
        occurredAtMillis: this.#now(snapshot),
        source: { kind: 'system', actorId: this.controllerId },
        kind: 'job.resumed',
        data: { interruptionSequence: snapshot.interruption.sequence },
      })
      return this.#append(event)
    })
  }

  async cancel(reasonInput: string): Promise<StrongFlowJobSnapshot> {
    const reason = nonEmptyReason(reasonInput, 'cancellation reason')
    this.#cancelRequested = reason
    this.#activeAbortController?.abort()
    return this.#serialize(async () => {
      try {
        const snapshot = (await this.store.read()).snapshot
        if (snapshot.state === 'CANCELLED') return snapshot
        if (TERMINAL_STATES.has(snapshot.state)) {
          throw new StrongFlowControllerError(
            'JOB_TERMINAL',
            `terminal job ${snapshot.state} cannot be cancelled`,
          )
        }
        return this.#cancelSnapshot(snapshot, reason)
      } finally {
        if (this.#cancelRequested === reason) this.#cancelRequested = undefined
      }
    })
  }

  async #runStage(
    stage: StrongFlowJobStage,
    snapshot: StrongFlowJobSnapshot,
    externalSignal?: AbortSignal,
  ): Promise<StrongFlowControllerAdvanceResult> {
    const provider = this.#provider(stage)
    const abortController = new AbortController()
    this.#activeAbortController = abortController
    const combinedSignal = combineSignals(abortController.signal, externalSignal)
    const signal = combinedSignal.signal
    try {
      if (signal.aborted) {
        const interrupted = await this.#interrupt(snapshot, `stage ${stage} was interrupted`)
        return Object.freeze({ kind: 'interrupted', snapshot: interrupted })
      }
      let stageRunId: StageRunIdentifier
      let attemptId: AttemptIdentifier
      let startedEvent: ReturnType<typeof createStrongFlowJobEvent<'stage.started'>>
      try {
        stageRunId = this.#stageRunIdFactory(stage, snapshot)
        attemptId = this.#attemptIdFactory(stage, snapshot)
        startedEvent = createStrongFlowJobEvent({
          jobId: snapshot.jobId,
          sequence: this.#nextSequence(snapshot),
          occurredAtMillis: this.#now(snapshot),
          source: { kind: 'role', actorId: provider.roleId },
          kind: 'stage.started',
          data: { stage, stageRunId, attemptId },
        })
      } catch (error) {
        throw new StrongFlowControllerError(
          'INVALID_CONTROLLER_OPTIONS',
          `controller could not create identities for stage ${stage}`,
          { cause: error },
        )
      }
      const startedSnapshot = await this.#append(startedEvent)
      if (signal.aborted) {
        const interrupted = await this.#interrupt(
          startedSnapshot,
          `stage ${stage} was interrupted`,
        )
        return Object.freeze({ kind: 'interrupted', snapshot: interrupted })
      }
      let result: unknown
      try {
        result = await provider.run(Object.freeze({
          stage,
          stageRunId,
          attemptId,
          snapshot: startedSnapshot,
          signal,
        }))
      } catch (error) {
        if (signal.aborted) {
          const interrupted = await this.#interrupt(
            startedSnapshot,
            `stage ${stage} was interrupted`,
          )
          return Object.freeze({ kind: 'interrupted', snapshot: interrupted })
        }
        const failure = error instanceof StrongFlowStageProviderFailure
          ? error
          : new StrongFlowStageProviderFailure({
            category: 'infrastructure',
            code: 'STAGE_PROVIDER_ERROR',
            message: `stage ${stage} provider failed unexpectedly`,
            retryable: false,
            cause: error,
          })
        return this.#settleStageFailure(startedSnapshot, provider.roleId, failure)
      }
      if (signal.aborted) {
        const interrupted = await this.#interrupt(
          startedSnapshot,
          `stage ${stage} was interrupted`,
        )
        return Object.freeze({ kind: 'interrupted', snapshot: interrupted })
      }

      let succeededEvent: ReturnType<typeof createStrongFlowJobEvent<'stage.succeeded'>>
      try {
        assertExactResult(result)
        const kernelSessionId = result.kernelSessionId === undefined
          ? undefined
          : KernelSessionId(result.kernelSessionId)
        succeededEvent = createStrongFlowJobEvent({
          jobId: startedSnapshot.jobId,
          sequence: this.#nextSequence(startedSnapshot),
          occurredAtMillis: this.#now(startedSnapshot),
          source: {
            kind: 'role',
            actorId: provider.roleId,
            ...(kernelSessionId === undefined ? {} : { kernelSessionId }),
          },
          kind: 'stage.succeeded',
          data: {
            stage,
            stageRunId,
            attemptId,
            ...result.output,
          } as StageSucceededData,
        })
      } catch (error) {
        const failure = new StrongFlowStageProviderFailure({
          category: 'task',
          code: 'INVALID_STAGE_RESULT',
          message: `stage ${stage} returned an invalid result`,
          retryable: false,
          cause: error,
        })
        return this.#settleStageFailure(startedSnapshot, provider.roleId, failure)
      }
      const settled = await this.#append(succeededEvent)
      return Object.freeze({ kind: 'stage-succeeded', stage, snapshot: settled })
    } finally {
      combinedSignal.dispose()
      if (this.#activeAbortController === abortController) {
        this.#activeAbortController = undefined
      }
    }
  }

  async #settleStageFailure(
    snapshot: StrongFlowJobSnapshot,
    roleId: string,
    failure: StrongFlowStageProviderFailure,
  ): Promise<StrongFlowControllerAdvanceResult> {
    const activeStage = snapshot.activeStage
    if (activeStage === undefined) {
      throw new StrongFlowControllerError(
        'INVALID_STAGE_RESULT',
        'cannot settle failure without an active stage',
      )
    }
    let kernelSessionId: KernelSessionIdentifier | undefined
    try {
      if (failure.kernelSessionId !== undefined) {
        kernelSessionId = KernelSessionId(String(failure.kernelSessionId))
      }
    } catch {
      kernelSessionId = undefined
    }
    const event = createStrongFlowJobEvent({
      jobId: snapshot.jobId,
      sequence: this.#nextSequence(snapshot),
      occurredAtMillis: this.#now(snapshot),
      source: {
        kind: 'role',
        actorId: roleId,
        ...(kernelSessionId === undefined ? {} : { kernelSessionId }),
      },
      kind: 'stage.failed',
      data: {
        stage: activeStage.stage,
        stageRunId: activeStage.stageRunId,
        attemptId: activeStage.attemptId,
        category: failure.category,
        code: failure.code,
        message: failure.message,
        retryable: failure.retryable,
      },
    })
    const settled = await this.#append(event)
    return Object.freeze({
      kind: 'stage-failed',
      stage: activeStage.stage,
      snapshot: settled,
    })
  }

  async #runCompletionGate(
    snapshot: StrongFlowJobSnapshot,
    externalSignal?: AbortSignal,
  ): Promise<StrongFlowControllerAdvanceResult> {
    if (snapshot.candidateId === undefined) {
      throw new StrongFlowControllerError(
        'INVALID_STAGE_RESULT',
        'completion gate has no current candidate',
      )
    }
    let stageRunId: StageRunIdentifier
    try {
      stageRunId = this.#stageRunIdFactory('COMPLETION_GATE', snapshot)
    } catch (error) {
      throw new StrongFlowControllerError(
        'INVALID_CONTROLLER_OPTIONS',
        'controller could not create a completion-gate identity',
        { cause: error },
      )
    }
    const abortController = new AbortController()
    this.#activeAbortController = abortController
    const combinedSignal = combineSignals(abortController.signal, externalSignal)
    const signal = combinedSignal.signal
    try {
      let result: StrongFlowCompletionGateResult
      try {
        result = await this.#completionGate.evaluate(Object.freeze({
          stageRunId,
          candidateId: snapshot.candidateId,
          snapshot,
          signal,
        }))
      } catch (error) {
        const interrupted = await this.#interrupt(
          snapshot,
          signal.aborted
            ? 'completion gate was interrupted'
            : 'completion gate failed before producing a result',
        )
        return Object.freeze({ kind: 'interrupted', snapshot: interrupted })
      }
      if (signal.aborted) {
        const interrupted = await this.#interrupt(snapshot, 'completion gate was interrupted')
        return Object.freeze({ kind: 'interrupted', snapshot: interrupted })
      }
      if (!isRecord(result) || !['passed', 'failed'].includes(String(result.outcome))) {
        const interrupted = await this.#interrupt(
          snapshot,
          'completion gate returned an invalid result',
        )
        return Object.freeze({ kind: 'interrupted', snapshot: interrupted })
      }
      if (result.outcome === 'passed' && Object.keys(result).some(key => key !== 'outcome')) {
        const interrupted = await this.#interrupt(
          snapshot,
          'completion gate returned an invalid result',
        )
        return Object.freeze({ kind: 'interrupted', snapshot: interrupted })
      }
      if (
        result.outcome === 'failed'
        && (
          typeof result.reason !== 'string'
          || result.reason.trim().length === 0
          || Object.keys(result).some(key => !['outcome', 'reason'].includes(key))
        )
      ) {
        const interrupted = await this.#interrupt(
          snapshot,
          'completion gate returned an invalid result',
        )
        return Object.freeze({ kind: 'interrupted', snapshot: interrupted })
      }
      const event = result.outcome === 'passed'
        ? createStrongFlowJobEvent({
          jobId: snapshot.jobId,
          sequence: this.#nextSequence(snapshot),
          occurredAtMillis: this.#now(snapshot),
          source: { kind: 'system', actorId: this.controllerId },
          kind: 'completion-gate.passed',
          data: { stageRunId, candidateId: snapshot.candidateId },
        })
        : createStrongFlowJobEvent({
          jobId: snapshot.jobId,
          sequence: this.#nextSequence(snapshot),
          occurredAtMillis: this.#now(snapshot),
          source: { kind: 'system', actorId: this.controllerId },
          kind: 'completion-gate.failed',
          data: {
            stageRunId,
            candidateId: snapshot.candidateId,
            reason: result.reason,
          },
        })
      const settled = await this.#append(event)
      return Object.freeze({
        kind: result.outcome === 'passed'
          ? 'completion-gate-passed'
          : 'completion-gate-failed',
        snapshot: settled,
      })
    } finally {
      combinedSignal.dispose()
      if (this.#activeAbortController === abortController) {
        this.#activeAbortController = undefined
      }
    }
  }

  async #recordDelivery(
    snapshot: StrongFlowJobSnapshot,
  ): Promise<StrongFlowControllerAdvanceResult> {
    if (snapshot.candidateId === undefined) {
      throw new StrongFlowControllerError(
        'INVALID_STAGE_RESULT',
        'delivery has no current candidate',
      )
    }
    const event = createStrongFlowJobEvent({
      jobId: snapshot.jobId,
      sequence: this.#nextSequence(snapshot),
      occurredAtMillis: this.#now(snapshot),
      source: { kind: 'system', actorId: this.controllerId },
      kind: 'job.delivered',
      data: { candidateId: snapshot.candidateId },
    })
    const delivered = await this.#append(event)
    return Object.freeze({ kind: 'delivered', snapshot: delivered })
  }

  async #cancelSnapshot(
    snapshot: StrongFlowJobSnapshot,
    reason: string,
  ): Promise<StrongFlowJobSnapshot> {
    const event = createStrongFlowJobEvent({
      jobId: snapshot.jobId,
      sequence: this.#nextSequence(snapshot),
      occurredAtMillis: this.#now(snapshot),
      source: { kind: 'system', actorId: this.controllerId },
      kind: 'job.cancelled',
      data: { reason },
    })
    return this.#append(event)
  }

  async #interrupt(snapshot: StrongFlowJobSnapshot, reason: string): Promise<StrongFlowJobSnapshot> {
    const event = createStrongFlowJobEvent({
      jobId: snapshot.jobId,
      sequence: this.#nextSequence(snapshot),
      occurredAtMillis: this.#now(snapshot),
      source: { kind: 'system', actorId: this.controllerId },
      kind: 'job.interrupted',
      data: {
        reason,
        ...(snapshot.activeStage === undefined
          ? {}
          : { stageRunId: snapshot.activeStage.stageRunId }),
      },
    })
    return this.#append(event)
  }

  #provider<Stage extends StrongFlowJobStage>(stage: Stage): StrongFlowStageProvider<Stage> {
    const provider = this.#providers.get(stage)
    if (provider === undefined) {
      throw new StrongFlowControllerError(
        'MISSING_STAGE_PROVIDER',
        `stage ${stage} has no provider`,
      )
    }
    return provider as unknown as StrongFlowStageProvider<Stage>
  }

  #passiveResult(
    snapshot: StrongFlowJobSnapshot,
  ): StrongFlowControllerAdvanceResult | undefined {
    if (TERMINAL_STATES.has(snapshot.state)) {
      return Object.freeze({ kind: 'terminal', snapshot })
    }
    if (snapshot.state === 'INTERRUPTED') {
      return Object.freeze({ kind: 'interrupted', snapshot })
    }
    if (snapshot.activeStage !== undefined) {
      return Object.freeze({ kind: 'active-stage', snapshot })
    }
    if (snapshot.state === 'AWAITING_HUMAN_REVIEW') {
      return Object.freeze({ kind: 'waiting-for-human-review', snapshot })
    }
    return undefined
  }

  #nextSequence(snapshot: StrongFlowJobSnapshot): string {
    return (BigInt(snapshot.sequence) + 1n).toString()
  }

  #now(snapshot: StrongFlowJobSnapshot): number {
    const value = this.#clock()
    if (!Number.isSafeInteger(value) || value < 0) {
      throw new StrongFlowControllerError(
        'INVALID_CONTROLLER_OPTIONS',
        'controller clock returned an invalid time',
      )
    }
    return Math.max(value, snapshot.lastOccurredAtMillis)
  }

  async #append<Event extends Parameters<StrongFlowJobStore['append']>[0]>(
    event: Event,
  ): Promise<StrongFlowJobSnapshot> {
    try {
      return await this.store.append(event)
    } catch (error) {
      if (
        error instanceof StrongFlowJobStoreError
        && ['EVENT_ALREADY_EXISTS', 'EVENT_SEQUENCE_MISMATCH'].includes(error.code)
      ) {
        throw new StrongFlowControllerError(
          'CONTROLLER_CONFLICT',
          'another controller changed the job first',
          { cause: error },
        )
      }
      throw error
    }
  }

  #serialize<Result>(operation: () => Promise<Result>): Promise<Result> {
    const current = this.#tail.then(operation, operation)
    this.#tail = current.then(() => {}, () => {})
    return current
  }
}
