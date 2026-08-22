import { randomUUID } from 'node:crypto'

import {
  DiagramId,
  HumanReviewId,
  RequirementId,
  SolutionId,
  createStrongFlowJobEvent,
  type DefinitionIdentity,
  type DefinitionRevisionScope,
  type HumanReviewChannel,
  type HumanReviewRecord,
  type HumanReviewId as HumanReviewIdentifier,
  type StrongFlowJobEvent,
  type StrongFlowJobSnapshot,
} from '@winwincode/contracts'

import {
  StrongFlowJobStore,
  StrongFlowJobStoreError,
} from './job-store.js'

export type HumanReviewGateErrorCode =
  | 'INVALID_REVIEW_REQUEST'
  | 'AUTHENTICATION_REQUIRED'
  | 'AUTHENTICATION_FAILED'
  | 'REVIEW_NOT_PENDING'
  | 'STALE_DEFINITION'
  | 'REVIEW_ALREADY_DECIDED'

/** Stable error returned before an untrusted caller can alter job state. */
export class HumanReviewGateError extends Error {
  readonly code: HumanReviewGateErrorCode

  constructor(code: HumanReviewGateErrorCode, message: string, options?: ErrorOptions) {
    super(message, options)
    this.name = 'HumanReviewGateError'
    this.code = code
  }
}

export interface HumanReviewAuthenticationRequest {
  readonly channel: HumanReviewChannel
  readonly authentication: unknown
}

export interface AuthenticatedHumanReviewer {
  readonly reviewerId: string
}

/** Implemented by the local UI session or explicit CLI peer-identity boundary. */
export interface HumanReviewAuthenticator {
  authenticate(
    request: HumanReviewAuthenticationRequest,
  ): Promise<AuthenticatedHumanReviewer | undefined>
}

interface HumanReviewSubmissionBase extends HumanReviewAuthenticationRequest {
  readonly definition: DefinitionIdentity
  readonly comment?: string
}

export type HumanReviewSubmission =
  | HumanReviewSubmissionBase & {
    readonly decision: 'approved'
  }
  | HumanReviewSubmissionBase & {
    readonly decision: 'changes-requested'
    readonly scope: DefinitionRevisionScope
  }
  | HumanReviewSubmissionBase & {
    readonly decision: 'rejected'
  }

type HumanReviewEvent = Extract<StrongFlowJobEvent, {
  readonly kind:
    | 'human-review.approved'
    | 'human-review.changes-requested'
    | 'human-review.rejected'
}>

export interface HumanReviewReceipt {
  readonly decision: HumanReviewRecord
  readonly event: HumanReviewEvent
  readonly snapshot: StrongFlowJobSnapshot
}

export interface HumanReviewGateOptions {
  readonly store: StrongFlowJobStore
  readonly authenticator: HumanReviewAuthenticator
  readonly clock?: () => number
  readonly reviewIdFactory?: () => HumanReviewIdentifier
}

interface HumanReviewWaiter {
  readonly resolve: (receipt: HumanReviewReceipt) => void
  readonly reject: (error: Error) => void
  readonly signal?: AbortSignal
  readonly abort?: () => void
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

function exactKeys(
  value: Record<string, unknown>,
  required: readonly string[],
  optional: readonly string[],
): void {
  const allowed = new Set([...required, ...optional])
  if (
    required.some(key => !Object.hasOwn(value, key))
    || Object.keys(value).some(key => !allowed.has(key))
  ) {
    throw new HumanReviewGateError(
      'INVALID_REVIEW_REQUEST',
      'human review request has an unexpected shape',
    )
  }
}

function parseDefinition(value: unknown): DefinitionIdentity {
  if (!isRecord(value)) {
    throw new HumanReviewGateError(
      'INVALID_REVIEW_REQUEST',
      'human review definition must be an object',
    )
  }
  exactKeys(value, [
    'requirementId',
    'solutionId',
    'systemArchitectureDiagramId',
    'processFlowDiagramId',
  ], [])
  try {
    if (
      typeof value.requirementId !== 'string'
      || typeof value.solutionId !== 'string'
      || typeof value.systemArchitectureDiagramId !== 'string'
      || typeof value.processFlowDiagramId !== 'string'
    ) throw new Error('definition identifiers must be strings')
    return Object.freeze({
      requirementId: RequirementId(value.requirementId),
      solutionId: SolutionId(value.solutionId),
      systemArchitectureDiagramId: DiagramId(value.systemArchitectureDiagramId),
      processFlowDiagramId: DiagramId(value.processFlowDiagramId),
    })
  } catch (error) {
    throw new HumanReviewGateError(
      'INVALID_REVIEW_REQUEST',
      'human review definition contains an invalid identifier',
      { cause: error },
    )
  }
}

function parseSubmission(value: unknown): HumanReviewSubmission {
  if (!isRecord(value)) {
    throw new HumanReviewGateError(
      'INVALID_REVIEW_REQUEST',
      'human review request must be an object',
    )
  }
  const changesRequested = value.decision === 'changes-requested'
  exactKeys(
    value,
    ['decision', 'channel', 'authentication', 'definition', ...(changesRequested ? ['scope'] : [])],
    ['comment'],
  )
  if (!['approved', 'changes-requested', 'rejected'].includes(String(value.decision))) {
    throw new HumanReviewGateError(
      'INVALID_REVIEW_REQUEST',
      'human review decision is invalid',
    )
  }
  if (!['local-ui', 'cli'].includes(String(value.channel))) {
    throw new HumanReviewGateError(
      'INVALID_REVIEW_REQUEST',
      'human review channel must be local-ui or cli',
    )
  }
  if (value.authentication === undefined) {
    throw new HumanReviewGateError(
      'AUTHENTICATION_REQUIRED',
      'human review authentication is required',
    )
  }
  if (value.comment !== undefined && typeof value.comment !== 'string') {
    throw new HumanReviewGateError(
      'INVALID_REVIEW_REQUEST',
      'human review comment must be a string when present',
    )
  }
  if (
    changesRequested
    && !['requirements', 'solution', 'diagrams'].includes(String(value.scope))
  ) {
    throw new HumanReviewGateError(
      'INVALID_REVIEW_REQUEST',
      'human review revision scope is invalid',
    )
  }
  const common = {
    channel: value.channel as HumanReviewChannel,
    authentication: value.authentication,
    definition: parseDefinition(value.definition),
    ...(value.comment === undefined ? {} : { comment: value.comment }),
  }
  if (value.decision === 'changes-requested') {
    return Object.freeze({
      ...common,
      decision: 'changes-requested',
      scope: value.scope as DefinitionRevisionScope,
    })
  }
  return Object.freeze({
    ...common,
    decision: value.decision as 'approved' | 'rejected',
  })
}

function completeDefinition(snapshot: StrongFlowJobSnapshot): DefinitionIdentity | undefined {
  const value = snapshot.definition
  if (
    value.requirementId === undefined
    || value.solutionId === undefined
    || value.systemArchitectureDiagramId === undefined
    || value.processFlowDiagramId === undefined
  ) return undefined
  return Object.freeze({
    requirementId: value.requirementId,
    solutionId: value.solutionId,
    systemArchitectureDiagramId: value.systemArchitectureDiagramId,
    processFlowDiagramId: value.processFlowDiagramId,
  })
}

function definitionsEqual(left: DefinitionIdentity, right: DefinitionIdentity): boolean {
  return left.requirementId === right.requirementId
    && left.solutionId === right.solutionId
    && left.systemArchitectureDiagramId === right.systemArchitectureDiagramId
    && left.processFlowDiagramId === right.processFlowDiagramId
}

function abortError(): Error {
  const error = new Error('human review wait was aborted')
  error.name = 'AbortError'
  return error
}

/** Authenticates, persists, and publishes the single human decision for a pending definition. */
export class StrongFlowHumanReviewGate {
  readonly store: StrongFlowJobStore
  readonly #authenticator: HumanReviewAuthenticator
  readonly #clock: () => number
  readonly #reviewIdFactory: () => HumanReviewIdentifier
  readonly #waiters = new Set<HumanReviewWaiter>()
  #tail: Promise<void> = Promise.resolve()

  constructor(options: HumanReviewGateOptions) {
    if (!(options.store instanceof StrongFlowJobStore)) {
      throw new HumanReviewGateError(
        'INVALID_REVIEW_REQUEST',
        'human review gate requires a StrongFlow job store',
      )
    }
    if (typeof options.authenticator?.authenticate !== 'function') {
      throw new HumanReviewGateError(
        'INVALID_REVIEW_REQUEST',
        'human review gate requires an authenticator',
      )
    }
    this.store = options.store
    this.#authenticator = options.authenticator
    this.#clock = options.clock ?? Date.now
    this.#reviewIdFactory = options.reviewIdFactory
      ?? (() => HumanReviewId(`review-${randomUUID()}`))
  }

  async submit(value: unknown): Promise<HumanReviewReceipt> {
    const submission = parseSubmission(value)
    let reviewer: AuthenticatedHumanReviewer | undefined
    try {
      reviewer = await this.#authenticator.authenticate({
        channel: submission.channel,
        authentication: submission.authentication,
      })
    } catch (error) {
      throw new HumanReviewGateError(
        'AUTHENTICATION_FAILED',
        'human reviewer authentication failed',
        { cause: error },
      )
    }
    if (reviewer === undefined) {
      throw new HumanReviewGateError(
        'AUTHENTICATION_REQUIRED',
        'human reviewer authentication was not accepted',
      )
    }

    return this.#serialize(async () => {
      const stored = await this.store.read()
      const currentDefinition = completeDefinition(stored.snapshot)
      if (stored.snapshot.state !== 'AWAITING_HUMAN_REVIEW') {
        const samePriorDecision = stored.snapshot.lastHumanReview !== undefined
          && definitionsEqual(
            stored.snapshot.lastHumanReview.payload.definition,
            submission.definition,
          )
        throw new HumanReviewGateError(
          samePriorDecision ? 'REVIEW_ALREADY_DECIDED' : 'REVIEW_NOT_PENDING',
          samePriorDecision
            ? 'the current definition already has a human decision'
            : 'the job is not waiting for human review',
        )
      }
      if (currentDefinition === undefined
        || !definitionsEqual(currentDefinition, submission.definition)) {
        throw new HumanReviewGateError(
          'STALE_DEFINITION',
          'human review does not match the current requirement, solution, and diagrams',
        )
      }
      if (
        typeof reviewer.reviewerId !== 'string'
        || reviewer.reviewerId.length === 0
        || reviewer.reviewerId.length > 200
        || reviewer.reviewerId.trim() !== reviewer.reviewerId
        || /[\u0000-\u001f\u007f]/u.test(reviewer.reviewerId)
      ) {
        throw new HumanReviewGateError(
          'AUTHENTICATION_FAILED',
          'authenticated reviewer identity is invalid',
        )
      }

      const now = this.#clock()
      if (!Number.isSafeInteger(now) || now < 0) {
        throw new HumanReviewGateError(
          'INVALID_REVIEW_REQUEST',
          'human review clock returned an invalid time',
        )
      }
      let reviewId: HumanReviewIdentifier
      try {
        reviewId = this.#reviewIdFactory()
      } catch (error) {
        throw new HumanReviewGateError(
          'INVALID_REVIEW_REQUEST',
          'human review id could not be created',
          { cause: error },
        )
      }

      let event: HumanReviewEvent
      try {
        event = this.#event(
          submission,
          reviewer.reviewerId,
          reviewId,
          (BigInt(stored.snapshot.sequence) + 1n).toString(),
          Math.max(now, stored.snapshot.lastOccurredAtMillis),
        )
      } catch (error) {
        throw new HumanReviewGateError(
          'INVALID_REVIEW_REQUEST',
          'human review event could not be created',
          { cause: error },
        )
      }
      let snapshot: StrongFlowJobSnapshot
      try {
        snapshot = await this.store.append(event)
      } catch (error) {
        if (
          error instanceof StrongFlowJobStoreError
          && error.code === 'EVENT_ALREADY_EXISTS'
        ) {
          const latest = await this.store.read()
          const latestDefinition = completeDefinition(latest.snapshot)
          if (
            latest.snapshot.lastHumanReview !== undefined
            && definitionsEqual(
              latest.snapshot.lastHumanReview.payload.definition,
              submission.definition,
            )
          ) {
            throw new HumanReviewGateError(
              'REVIEW_ALREADY_DECIDED',
              'another human decision was published first',
              { cause: error },
            )
          }
          throw new HumanReviewGateError(
            latestDefinition === undefined
              || !definitionsEqual(latestDefinition, submission.definition)
              ? 'STALE_DEFINITION'
              : 'REVIEW_NOT_PENDING',
            'job state changed before the human decision was published',
            { cause: error },
          )
        }
        throw error
      }
      const decision = snapshot.lastHumanReview
      if (decision === undefined || decision.artifactId !== reviewId) {
        throw new HumanReviewGateError(
          'REVIEW_NOT_PENDING',
          'persisted review did not produce the expected decision',
        )
      }
      const receipt = Object.freeze({ decision, event, snapshot })
      this.#resolveWaiters(receipt)
      return receipt
    })
  }

  /** Waits without polling, timers, kernel turns, or model calls. */
  async waitForDecision(signal?: AbortSignal): Promise<HumanReviewReceipt> {
    if (signal?.aborted === true) throw abortError()
    const holder = await this.#serialize(async () => {
      const stored = await this.store.read()
      if (stored.snapshot.state !== 'AWAITING_HUMAN_REVIEW') {
        throw new HumanReviewGateError(
          'REVIEW_NOT_PENDING',
          'the job is not waiting for human review',
        )
      }
      let resolveWaiter: (receipt: HumanReviewReceipt) => void = () => {}
      let rejectWaiter: (error: Error) => void = () => {}
      const promise = new Promise<HumanReviewReceipt>((resolvePromise, rejectPromise) => {
        resolveWaiter = resolvePromise
        rejectWaiter = rejectPromise
      })
      const waiter: HumanReviewWaiter = {
        resolve: resolveWaiter,
        reject: rejectWaiter,
        ...(signal === undefined ? {} : { signal }),
        ...(signal === undefined
          ? {}
          : {
            abort: () => {
              if (!this.#waiters.delete(waiter)) return
              waiter.reject(abortError())
            },
          }),
      }
      this.#waiters.add(waiter)
      if (waiter.abort !== undefined) {
        signal?.addEventListener('abort', waiter.abort, { once: true })
        if (signal?.aborted === true) waiter.abort()
      }
      return { promise }
    })
    return holder.promise
  }

  #event(
    submission: HumanReviewSubmission,
    reviewerId: string,
    reviewId: HumanReviewIdentifier,
    sequence: string,
    occurredAtMillis: number,
  ): HumanReviewEvent {
    const common = {
      jobId: this.store.manifest.jobId,
      sequence,
      occurredAtMillis,
      source: {
        kind: 'human' as const,
        actorId: reviewerId,
        channel: submission.channel,
      },
    }
    const data = {
      reviewId,
      reviewerId,
      definition: submission.definition,
      ...(submission.comment === undefined ? {} : { comment: submission.comment }),
    }
    if (submission.decision === 'approved') {
      return createStrongFlowJobEvent({
        ...common,
        kind: 'human-review.approved',
        data,
      })
    }
    if (submission.decision === 'rejected') {
      return createStrongFlowJobEvent({
        ...common,
        kind: 'human-review.rejected',
        data,
      })
    }
    return createStrongFlowJobEvent({
      ...common,
      kind: 'human-review.changes-requested',
      data: { ...data, scope: submission.scope },
    })
  }

  #resolveWaiters(receipt: HumanReviewReceipt): void {
    for (const waiter of this.#waiters) {
      if (waiter.abort !== undefined) {
        waiter.signal?.removeEventListener('abort', waiter.abort)
      }
      waiter.resolve(receipt)
    }
    this.#waiters.clear()
  }

  #serialize<Result>(operation: () => Promise<Result>): Promise<Result> {
    const current = this.#tail.then(operation, operation)
    this.#tail = current.then(() => {}, () => {})
    return current
  }
}
