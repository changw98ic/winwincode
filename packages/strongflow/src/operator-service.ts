import { createHash, randomUUID, timingSafeEqual } from 'node:crypto'
import {
  link,
  mkdir,
  open,
  readFile,
  rm,
} from 'node:fs/promises'
import { dirname, join, resolve } from 'node:path'
import { isDeepStrictEqual } from 'node:util'

import {
  HumanReviewId,
  JobId,
  STRONGFLOW_OPERATOR_ARTIFACT_LINK_SCHEMA_VERSION,
  STRONGFLOW_OPERATOR_EVENT_SCHEMA_VERSION,
  STRONGFLOW_OPERATOR_EXPORT_SCHEMA_VERSION,
  STRONGFLOW_OPERATOR_JOB_VIEW_SCHEMA_VERSION,
  STRONGFLOW_OPERATOR_MAX_EXPORT_ARTIFACTS,
  STRONGFLOW_OPERATOR_MAX_EXPORT_EVENTS,
  STRONGFLOW_OPERATOR_MUTATING_OPERATIONS,
  STRONGFLOW_OPERATOR_OPERATIONS,
  STRONGFLOW_ROLE_IDS,
  UserRequestId,
  applyStrongFlowJobEvent,
  createStrongFlowJobEvent,
  materializeStrongFlowArtifact,
  materializeStrongFlowOperatorFailure,
  materializeStrongFlowOperatorSuccess,
  parseStrongFlowOperatorEventCursor,
  parseStrongFlowOperatorRequest,
  parseStrongFlowOperatorResponse,
  parseStrongFlowOperatorResponseForRequest,
  projectStrongFlowJob,
  strongFlowOperatorEventCursor,
  type DefinitionIdentity,
  type HumanReviewChannel,
  type JobId as JobIdentifier,
  type StrongFlowArtifact,
  type StrongFlowJobEvent,
  type StrongFlowJobSnapshot,
  type StrongFlowOperatorArtifactLink,
  type StrongFlowOperatorEventPage,
  type StrongFlowOperatorEventSource,
  type StrongFlowOperatorEventView,
  type StrongFlowOperatorInvoker,
  type StrongFlowOperatorJobView,
  type StrongFlowOperatorOperation,
  type StrongFlowOperatorRequest,
  type StrongFlowOperatorRequestFor,
  type StrongFlowOperatorResponse,
  type StrongFlowOperatorResponseFor,
  type StrongFlowOperatorStageRunView,
} from '@winwincode/contracts'

import {
  StrongFlowArtifactStore,
  StrongFlowArtifactStoreError,
  type StrongFlowArtifactStoreArtifactRecord,
  type StrongFlowStoredCandidateReference,
} from './artifact-store.js'
import {
  HumanReviewGateError,
  StrongFlowHumanReviewGate,
  type HumanReviewAuthenticator,
  type HumanReviewReceipt,
} from './human-review-gate.js'
import {
  StrongFlowJobStore,
  StrongFlowJobStoreError,
  type StrongFlowStoredJob,
} from './job-store.js'
import { containsStrongFlowCredentialMaterial } from './security-audit.js'

export const STRONGFLOW_OPERATOR_SERVICE_SCHEMA_VERSION = 1 as const

const OPERATOR_ROOT = 'strongflow-operator'
const REQUEST_RECEIPT_ROOT = 'requests'
const REQUEST_CLAIM_ROOT = 'request-claims'
const JOB_CONFIGURATION_ROOT = 'jobs'
const HASH_PATTERN = /^[0-9a-f]{64}$/u
const PORTABLE_ACTOR_PATTERN = /^[A-Za-z0-9][A-Za-z0-9._:/@-]{0,499}$/u
const TERMINAL_STATES = new Set(['FAILED', 'REJECTED', 'CANCELLED', 'DELIVERED'])
const MUTATING_OPERATIONS = new Set<StrongFlowOperatorOperation>(
  STRONGFLOW_OPERATOR_MUTATING_OPERATIONS,
)

export type StrongFlowOperatorServiceErrorCode =
  | 'INVALID_SERVICE_OPTIONS'
  | 'JOB_CONFLICT'
  | 'WRONG_JOB_STATE'
  | 'JOB_TERMINAL'
  | 'STORE_FAILURE'

/** A stable local-service failure that never carries request or authentication content. */
export class StrongFlowOperatorServiceError extends Error {
  readonly code: StrongFlowOperatorServiceErrorCode

  constructor(
    code: StrongFlowOperatorServiceErrorCode,
    message: string,
    options?: ErrorOptions,
  ) {
    super(message, options)
    this.name = 'StrongFlowOperatorServiceError'
    this.code = code
  }
}

/** Durable creation data needed by a worker without copying the user request body. */
export interface StrongFlowOperatorJobConfiguration {
  readonly schemaVersion: typeof STRONGFLOW_OPERATOR_SERVICE_SCHEMA_VERSION
  readonly jobId: JobIdentifier
  readonly requestId: string
  readonly requestSha256: string
  readonly repositoryPath: string
  readonly baseRevision: string | null
  readonly title: string | null
  readonly userRequestArtifactId: string
  readonly createdAtMillis: number
}

export interface StrongFlowOperatorRunnableJob {
  readonly configuration: StrongFlowOperatorJobConfiguration
  readonly jobStore: StrongFlowJobStore
  readonly artifactStore: StrongFlowArtifactStore
}

/** Optional worker notification. Returning from an operator call never waits for role execution. */
export interface StrongFlowOperatorJobScheduler {
  jobReady(job: StrongFlowOperatorRunnableJob): Promise<void> | void
  /** Stop active work after the cancellation event is already durable. */
  jobCancelled?(jobId: JobIdentifier, reason: string): Promise<void> | void
}

export interface StrongFlowOperatorServiceOptions {
  readonly home: string
  readonly authenticator: HumanReviewAuthenticator
  readonly scheduler?: StrongFlowOperatorJobScheduler
  readonly clock?: () => number
  readonly followPollMillis?: number
}

export interface StrongFlowLocalProofAuthenticatorOptions {
  readonly localSessionProof?: string
  readonly localPeerProof?: string
  readonly localSessionReviewerId?: string
  readonly localPeerReviewerId?: string
}

interface StoredRequestReceipt {
  readonly schemaVersion: typeof STRONGFLOW_OPERATOR_SERVICE_SCHEMA_VERSION
  readonly requestId: string
  readonly operation: StrongFlowOperatorOperation
  readonly requestSha256: string
  readonly response: StrongFlowOperatorResponse
}

interface StoredRequestClaim {
  readonly schemaVersion: typeof STRONGFLOW_OPERATOR_SERVICE_SCHEMA_VERSION
  readonly requestId: string
  readonly operation: StrongFlowOperatorOperation
  readonly requestSha256: string
}

interface OpenedOperatorJob {
  readonly jobStore: StrongFlowJobStore
  readonly artifacts: StrongFlowArtifactStore
  readonly job: StrongFlowStoredJob
}

class OperatorFault extends Error {
  readonly code:
    | 'INVALID_REQUEST'
    | 'JOB_NOT_FOUND'
    | 'ARTIFACT_NOT_FOUND'
    | 'JOB_CONFLICT'
    | 'WRONG_JOB_STATE'
    | 'JOB_TERMINAL'
    | 'STALE_DEFINITION'
    | 'REVIEW_ALREADY_DECIDED'
    | 'AUTHENTICATION_REQUIRED'
    | 'AUTHENTICATION_FAILED'
    | 'OPERATION_ABORTED'
    | 'STORE_FAILURE'
    | 'INTERNAL_ERROR'
    | 'LIMIT_EXCEEDED'
    | 'INVALID_CURSOR'
  readonly field: string | null
  readonly currentDefinition: DefinitionIdentity | null

  constructor(
    code: OperatorFault['code'],
    message: string,
    options: {
      readonly field?: string | null
      readonly currentDefinition?: DefinitionIdentity | null
      readonly cause?: unknown
    } = {},
  ) {
    super(message, options.cause === undefined ? undefined : { cause: options.cause })
    this.name = 'OperatorFault'
    this.code = code
    this.field = options.field ?? null
    this.currentDefinition = options.currentDefinition ?? null
  }
}

function isRecord(value: unknown): value is Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return false
  const prototype = Object.getPrototypeOf(value)
  return prototype === Object.prototype || prototype === null
}

function errorCode(error: unknown): string | undefined {
  if (typeof error !== 'object' || error === null || !('code' in error)) return undefined
  return typeof error.code === 'string' ? error.code : undefined
}

function exactKeys(value: Record<string, unknown>, expected: readonly string[], label: string): void {
  const keys = Object.keys(value)
  if (keys.length !== expected.length
    || expected.some(key => !Object.hasOwn(value, key))
    || keys.some(key => !expected.includes(key))) {
    throw new Error(`${label} has an unexpected shape`)
  }
}

function sha256(value: string): string {
  return createHash('sha256').update(value).digest('hex')
}

function requestSha256(request: object): string {
  return sha256(JSON.stringify(request))
}

function requestActorId(requestId: string): string {
  return `operator-request-${sha256(requestId).slice(0, 40)}`
}

function operatorActorId(value: string): string {
  return PORTABLE_ACTOR_PATTERN.test(value)
    ? value
    : `actor-sha256-${sha256(value)}`
}

function deterministicJobId(requestId: string): JobIdentifier {
  return JobId(`job-${sha256(requestId).slice(0, 40)}`)
}

function deterministicUserRequestId(requestId: string): string {
  return UserRequestId(`user-request-${sha256(requestId).slice(0, 40)}`)
}

function deterministicReviewId(requestId: string): ReturnType<typeof HumanReviewId> {
  return HumanReviewId(`review-${sha256(requestId).slice(0, 40)}`)
}

function validateClock(clock: () => number): number {
  const value = clock()
  if (!Number.isSafeInteger(value) || value < 0) {
    throw new StrongFlowOperatorServiceError(
      'INVALID_SERVICE_OPTIONS',
      'StrongFlow operator clock returned an invalid time',
    )
  }
  return value
}

function nextSequence(snapshot: StrongFlowJobSnapshot): string {
  return (BigInt(snapshot.sequence) + 1n).toString()
}

function completeDefinition(snapshot: StrongFlowJobSnapshot): DefinitionIdentity | null {
  const definition = snapshot.definition
  if (definition.requirementId === undefined
    || definition.solutionId === undefined
    || definition.systemArchitectureDiagramId === undefined
    || definition.processFlowDiagramId === undefined) return null
  return Object.freeze({
    requirementId: definition.requirementId,
    solutionId: definition.solutionId,
    systemArchitectureDiagramId: definition.systemArchitectureDiagramId,
    processFlowDiagramId: definition.processFlowDiagramId,
  })
}

function definitionsEqual(left: DefinitionIdentity, right: DefinitionIdentity): boolean {
  return left.requirementId === right.requirementId
    && left.solutionId === right.solutionId
    && left.systemArchitectureDiagramId === right.systemArchitectureDiagramId
    && left.processFlowDiagramId === right.processFlowDiagramId
}

type HumanReviewEvent = Extract<StrongFlowJobEvent, {
  readonly kind:
    | 'human-review.approved'
    | 'human-review.changes-requested'
    | 'human-review.rejected'
}>

function recoverHumanReviewReceipt(
  events: readonly StrongFlowJobEvent[],
  reviewId: ReturnType<typeof HumanReviewId>,
  decision: 'approved' | 'rejected' | 'changes-requested',
  definition: DefinitionIdentity,
): HumanReviewReceipt | undefined {
  const event = events.find((candidate): candidate is HumanReviewEvent => (
    (candidate.kind === 'human-review.approved'
      || candidate.kind === 'human-review.changes-requested'
      || candidate.kind === 'human-review.rejected')
    && candidate.data.reviewId === reviewId
  ))
  if (event === undefined) return undefined
  const index = events.indexOf(event)
  const snapshot = projectStrongFlowJob(events.slice(0, index + 1))
  const review = snapshot.lastHumanReview
  if (review === undefined || review.artifactId !== reviewId) {
    throw new OperatorFault('JOB_CONFLICT', '人工审核恢复记录不一致。')
  }
  if (review.payload.decision !== decision
    || !definitionsEqual(review.payload.definition, definition)) {
    throw new OperatorFault('JOB_CONFLICT', '人工审核请求身份对应另一项决定。')
  }
  return Object.freeze({ decision: review, event, snapshot })
}

function constantTimeEqual(left: string, right: string): boolean {
  const leftDigest = createHash('sha256').update(left).digest()
  const rightDigest = createHash('sha256').update(right).digest()
  return timingSafeEqual(leftDigest, rightDigest)
}

function reviewerId(value: string, label: string): string {
  if (value.length === 0
    || value.length > 200
    || value.trim() !== value
    || /[\u0000-\u001f\u007f]/u.test(value)) {
    throw new StrongFlowOperatorServiceError(
      'INVALID_SERVICE_OPTIONS',
      `${label} is not a valid reviewer identity`,
    )
  }
  return value
}

/** Exact local proof matcher used by the packaged Host and CLI process. */
export function createStrongFlowLocalProofAuthenticator(
  options: StrongFlowLocalProofAuthenticatorOptions,
): HumanReviewAuthenticator {
  const localSessionReviewerId = reviewerId(
    options.localSessionReviewerId ?? 'local-ui-reviewer',
    'localSessionReviewerId',
  )
  const localPeerReviewerId = reviewerId(
    options.localPeerReviewerId ?? 'local-cli-reviewer',
    'localPeerReviewerId',
  )
  return Object.freeze({
    async authenticate(request: Parameters<HumanReviewAuthenticator['authenticate']>[0]) {
      if (!isRecord(request.authentication)
        || typeof request.authentication.scheme !== 'string'
        || typeof request.authentication.proof !== 'string') return undefined
      if (request.channel === 'local-ui'
        && request.authentication.scheme === 'local-session'
        && options.localSessionProof !== undefined
        && constantTimeEqual(request.authentication.proof, options.localSessionProof)) {
        return Object.freeze({ reviewerId: localSessionReviewerId })
      }
      if (request.channel === 'cli'
        && request.authentication.scheme === 'local-peer'
        && options.localPeerProof !== undefined
        && constantTimeEqual(request.authentication.proof, options.localPeerProof)) {
        return Object.freeze({ reviewerId: localPeerReviewerId })
      }
      return undefined
    },
  })
}

function operatorRoot(home: string): string {
  return join(home, OPERATOR_ROOT)
}

function requestReceiptPath(home: string, requestId: string): string {
  return join(operatorRoot(home), REQUEST_RECEIPT_ROOT, `${sha256(requestId)}.json`)
}

function requestClaimPath(home: string, requestId: string): string {
  return join(operatorRoot(home), REQUEST_CLAIM_ROOT, `${sha256(requestId)}.json`)
}

function jobConfigurationPath(home: string, jobId: string): string {
  return join(operatorRoot(home), JOB_CONFIGURATION_ROOT, `${sha256(jobId)}.json`)
}

async function syncDirectory(path: string): Promise<void> {
  const handle = await open(path, 'r')
  try {
    await handle.sync()
  } finally {
    await handle.close()
  }
}

async function writeExclusiveJson(path: string, value: unknown): Promise<boolean> {
  const directory = dirname(path)
  await mkdir(directory, { recursive: true, mode: 0o700 })
  const temporary = join(
    directory,
    `.pending-${sha256(path).slice(0, 16)}-${process.pid}-${randomUUID()}`,
  )
  const handle = await open(temporary, 'wx', 0o600)
  try {
    await handle.writeFile(`${JSON.stringify(value)}\n`, 'utf8')
    await handle.sync()
  } finally {
    await handle.close()
  }
  try {
    await link(temporary, path)
    await syncDirectory(directory)
    return true
  } catch (error) {
    if (errorCode(error) === 'EEXIST') return false
    throw error
  } finally {
    await rm(temporary, { force: true })
  }
}

async function readJson(path: string): Promise<unknown | undefined> {
  let text: string
  try {
    text = await readFile(path, 'utf8')
  } catch (error) {
    if (errorCode(error) === 'ENOENT') return undefined
    throw error
  }
  if (!text.endsWith('\n') || text.slice(0, -1).includes('\n')) {
    throw new Error('operator record is incomplete or has extra records')
  }
  return JSON.parse(text.slice(0, -1)) as unknown
}

function parseJobConfiguration(value: unknown): StrongFlowOperatorJobConfiguration {
  if (!isRecord(value)) throw new Error('job configuration must be an object')
  exactKeys(value, [
    'schemaVersion',
    'jobId',
    'requestId',
    'requestSha256',
    'repositoryPath',
    'baseRevision',
    'title',
    'userRequestArtifactId',
    'createdAtMillis',
  ], 'job configuration')
  if (value.schemaVersion !== STRONGFLOW_OPERATOR_SERVICE_SCHEMA_VERSION
    || typeof value.jobId !== 'string'
    || typeof value.requestId !== 'string'
    || typeof value.requestSha256 !== 'string'
    || !HASH_PATTERN.test(value.requestSha256)
    || typeof value.repositoryPath !== 'string'
    || (value.baseRevision !== null && typeof value.baseRevision !== 'string')
    || (value.title !== null && typeof value.title !== 'string')
    || typeof value.userRequestArtifactId !== 'string'
    || !Number.isSafeInteger(value.createdAtMillis)
    || Number(value.createdAtMillis) < 0) throw new Error('job configuration is invalid')
  return Object.freeze({
    schemaVersion: STRONGFLOW_OPERATOR_SERVICE_SCHEMA_VERSION,
    jobId: JobId(value.jobId),
    requestId: value.requestId,
    requestSha256: value.requestSha256,
    repositoryPath: resolve(value.repositoryPath),
    baseRevision: value.baseRevision,
    title: value.title,
    userRequestArtifactId: UserRequestId(value.userRequestArtifactId),
    createdAtMillis: Number(value.createdAtMillis),
  })
}

function parseStoredRequestReceipt(value: unknown): StoredRequestReceipt {
  if (!isRecord(value)) throw new Error('request receipt must be an object')
  exactKeys(value, [
    'schemaVersion',
    'requestId',
    'operation',
    'requestSha256',
    'response',
  ], 'request receipt')
  if (value.schemaVersion !== STRONGFLOW_OPERATOR_SERVICE_SCHEMA_VERSION
    || typeof value.requestId !== 'string'
    || typeof value.operation !== 'string'
    || !STRONGFLOW_OPERATOR_OPERATIONS.includes(
      value.operation as StrongFlowOperatorOperation,
    )
    || typeof value.requestSha256 !== 'string'
    || !HASH_PATTERN.test(value.requestSha256)) throw new Error('request receipt is invalid')
  const response = parseStrongFlowOperatorResponse(value.response)
  if (response.requestId !== value.requestId || response.operation !== value.operation) {
    throw new Error('request receipt response identity is invalid')
  }
  return Object.freeze({
    schemaVersion: STRONGFLOW_OPERATOR_SERVICE_SCHEMA_VERSION,
    requestId: value.requestId,
    operation: value.operation as StrongFlowOperatorOperation,
    requestSha256: value.requestSha256,
    response,
  })
}

function parseStoredRequestClaim(value: unknown): StoredRequestClaim {
  if (!isRecord(value)) throw new Error('request claim must be an object')
  exactKeys(value, [
    'schemaVersion',
    'requestId',
    'operation',
    'requestSha256',
  ], 'request claim')
  if (value.schemaVersion !== STRONGFLOW_OPERATOR_SERVICE_SCHEMA_VERSION
    || typeof value.requestId !== 'string'
    || typeof value.operation !== 'string'
    || !STRONGFLOW_OPERATOR_OPERATIONS.includes(
      value.operation as StrongFlowOperatorOperation,
    )
    || typeof value.requestSha256 !== 'string'
    || !HASH_PATTERN.test(value.requestSha256)) throw new Error('request claim is invalid')
  return Object.freeze({
    schemaVersion: STRONGFLOW_OPERATOR_SERVICE_SCHEMA_VERSION,
    requestId: value.requestId,
    operation: value.operation as StrongFlowOperatorOperation,
    requestSha256: value.requestSha256,
  })
}

function storedCandidate(
  value: StrongFlowStoredCandidateReference | null,
): StrongFlowOperatorArtifactLink['candidate'] {
  if (value === null) return null
  return value.kind === 'complete'
    ? Object.freeze({ kind: 'complete', identity: value.identity })
    : Object.freeze({
      kind: 'diff',
      candidateId: value.candidateId,
      diffId: value.diffId,
    })
}

function artifactLink(record: StrongFlowArtifactStoreArtifactRecord): StrongFlowOperatorArtifactLink {
  const producer: StrongFlowOperatorArtifactLink['producer'] = record.producer.kind === 'role'
    ? Object.freeze({
      kind: 'role',
      roleId: record.producer.roleId,
      stageRunId: record.producer.stageRunId,
      attemptId: record.producer.attemptId,
      kernelSessionId: record.producer.eventInterval.kernelSessionId,
      firstKernelSequence: record.producer.eventInterval.firstSequence,
      lastKernelSequence: record.producer.eventInterval.lastSequence,
      kernelEventCount: record.producer.eventInterval.eventCount,
    })
    : record.producer.kind === 'human'
      ? Object.freeze({
        kind: 'human',
        actorId: operatorActorId(record.producer.actorId),
        channel: record.producer.channel,
      })
      : Object.freeze({
        kind: 'system',
        actorId: operatorActorId(record.producer.actorId),
      })
  return Object.freeze({
    schemaVersion: STRONGFLOW_OPERATOR_ARTIFACT_LINK_SCHEMA_VERSION,
    jobId: record.jobId,
    sequence: record.sequence,
    recordId: record.recordId,
    artifactKind: record.identity.artifactKind,
    artifactId: record.identity.artifactId,
    blobId: record.blob.blobId,
    byteLength: record.blob.byteLength,
    mediaType: record.blob.mediaType,
    createdAtMillis: record.createdAtMillis,
    producer,
    candidate: storedCandidate(record.candidate),
  })
}

function roleForStage(stage: StrongFlowOperatorStageRunView['stage']): StrongFlowOperatorStageRunView['roleId'] {
  switch (stage) {
    case 'REQUIREMENTS': return 'requirements'
    case 'SOLUTION':
    case 'DIAGRAMS': return 'solution'
    case 'PLANNING': return 'planner'
    case 'EXECUTION': return 'executor'
    case 'VERIFICATION': return 'verifier'
    case 'REMEDIATION': return 'remediator'
    case 'DELIVERY': return 'verifier'
  }
}

function canonicalRole(
  actorId: string,
  stage: StrongFlowOperatorStageRunView['stage'],
): StrongFlowOperatorStageRunView['roleId'] {
  return STRONGFLOW_ROLE_IDS.includes(actorId as typeof STRONGFLOW_ROLE_IDS[number])
    ? actorId as typeof STRONGFLOW_ROLE_IDS[number]
    : roleForStage(stage)
}

function eventStage(
  event: StrongFlowJobEvent,
  events: readonly StrongFlowJobEvent[],
): StrongFlowOperatorStageRunView | null {
  let stageRunId: string | undefined
  let stage: StrongFlowOperatorStageRunView['stage'] | undefined
  let attemptId: string | undefined
  if (event.kind === 'stage.started'
    || event.kind === 'stage.succeeded'
    || event.kind === 'stage.failed') {
    stageRunId = event.data.stageRunId
    stage = event.data.stage
    attemptId = event.data.attemptId
  } else if (event.kind === 'job.interrupted' && event.data.stageRunId !== undefined) {
    stageRunId = event.data.stageRunId
  }
  if (stageRunId === undefined) return null
  let started: Extract<StrongFlowJobEvent, { readonly kind: 'stage.started' }> | undefined
  for (const entry of events) {
    if (entry.kind === 'stage.started' && entry.data.stageRunId === stageRunId) {
      started = entry
      break
    }
  }
  if (started !== undefined) {
    stage = started.data.stage
    attemptId = started.data.attemptId
  }
  if (stage === undefined || attemptId === undefined) return null
  const actorId = started?.source.actorId ?? event.source.actorId
  return Object.freeze({
    stage,
    stageRunId,
    attemptId,
    roleId: canonicalRole(actorId, stage),
    kernelSessionId: event.source.kind === 'role'
      ? event.source.kernelSessionId ?? null
      : null,
    startedAtMillis: started?.occurredAtMillis ?? event.occurredAtMillis,
  })
}

function operatorEventSource(
  event: StrongFlowJobEvent,
  stage: StrongFlowOperatorStageRunView | null,
): StrongFlowOperatorEventSource {
  if (event.source.kind === 'system') {
    return Object.freeze({
      kind: 'system',
      actorId: operatorActorId(event.source.actorId),
    })
  }
  if (event.source.kind === 'human') {
    return Object.freeze({
      kind: 'human',
      actorId: operatorActorId(event.source.actorId),
      channel: event.source.channel,
    })
  }
  return Object.freeze({
    kind: 'role',
    actorId: operatorActorId(event.source.actorId),
    roleId: stage?.roleId ?? 'requirements',
    kernelSessionId: event.source.kernelSessionId ?? null,
  })
}

function reviewStatus(snapshot: StrongFlowJobSnapshot): StrongFlowOperatorJobView['review'] {
  const definition = completeDefinition(snapshot)
  if (snapshot.state === 'AWAITING_HUMAN_REVIEW' && definition !== null) {
    return Object.freeze({ status: 'pending', definition, record: null })
  }
  if (snapshot.approval !== undefined && definition !== null) {
    return Object.freeze({
      status: 'approved',
      definition,
      record: snapshot.approval,
    })
  }
  const review = snapshot.lastHumanReview
  if (review?.payload.decision === 'changes-requested') {
    return Object.freeze({
      status: 'changes-requested',
      definition: review.payload.definition,
      record: review,
    })
  }
  if (review?.payload.decision === 'rejected') {
    return Object.freeze({
      status: 'rejected',
      definition: review.payload.definition,
      record: review,
    })
  }
  return Object.freeze({ status: 'unavailable', definition: null, record: null })
}

function allowedOperations(snapshot: StrongFlowJobSnapshot): readonly StrongFlowOperatorOperation[] {
  const operations: StrongFlowOperatorOperation[] = [
    'job.status',
    'job.follow',
    'job.artifacts',
    'job.export',
  ]
  if (TERMINAL_STATES.has(snapshot.state)) return Object.freeze(operations)
  operations.push('job.cancel')
  if (snapshot.state === 'INTERRUPTED') {
    operations.push('job.resume')
    return Object.freeze(operations)
  }
  if (snapshot.state === 'AWAITING_HUMAN_REVIEW') {
    operations.push('review.approve', 'review.reject', 'review.request-changes')
  }
  return Object.freeze(operations)
}

function executionLock(snapshot: StrongFlowJobSnapshot): StrongFlowOperatorJobView['executionLock'] {
  if (TERMINAL_STATES.has(snapshot.state)) {
    return Object.freeze({
      locked: true,
      reason: 'job-terminal',
      message: '作业已经结束，不能继续执行。',
    })
  }
  if (snapshot.state === 'INTERRUPTED') {
    return Object.freeze({
      locked: true,
      reason: 'job-interrupted',
      message: '作业已中断，需要按准确中断序号恢复。',
    })
  }
  if (snapshot.state === 'AWAITING_HUMAN_REVIEW') {
    return Object.freeze({
      locked: true,
      reason: 'awaiting-human-review',
      message: '需求、方案和两张图正在等待人工审核。',
    })
  }
  if (completeDefinition(snapshot) === null) {
    const revisionRequested = snapshot.lastHumanReview?.payload.decision === 'changes-requested'
    return Object.freeze({
      locked: true,
      reason: revisionRequested ? 'definition-revision-requested' : 'definition-incomplete',
      message: revisionRequested
        ? '人工已要求修改，必须先生成新的完整定义。'
        : '需求、方案和两张定义图尚未全部生成。',
    })
  }
  if (snapshot.approval !== undefined && snapshot.state === 'PLANNING') {
    return Object.freeze({
      locked: false,
      reason: 'definition-approved',
      message: '当前四个定义身份已经人工批准。',
    })
  }
  return Object.freeze({
    locked: false,
    reason: 'job-active',
    message: '作业正在按已批准定义流转。',
  })
}

function activeStage(snapshot: StrongFlowJobSnapshot): StrongFlowOperatorStageRunView | null {
  const stage = snapshot.activeStage
  if (stage === undefined) return null
  return Object.freeze({
    stage: stage.stage,
    stageRunId: stage.stageRunId,
    attemptId: stage.attemptId,
    roleId: canonicalRole(stage.actorId, stage.stage),
    kernelSessionId: stage.kernelSessionId ?? null,
    startedAtMillis: stage.startedAtMillis,
  })
}

function jobView(snapshot: StrongFlowJobSnapshot): StrongFlowOperatorJobView {
  const definition = snapshot.definition
  return Object.freeze({
    schemaVersion: STRONGFLOW_OPERATOR_JOB_VIEW_SCHEMA_VERSION,
    jobId: snapshot.jobId,
    title: snapshot.title ?? null,
    state: snapshot.state,
    sequence: snapshot.sequence,
    updatedAtMillis: snapshot.lastOccurredAtMillis,
    definition: Object.freeze({
      revision: snapshot.definitionRevision,
      requirementId: definition.requirementId ?? null,
      solutionId: definition.solutionId ?? null,
      systemArchitectureDiagramId: definition.systemArchitectureDiagramId ?? null,
      processFlowDiagramId: definition.processFlowDiagramId ?? null,
    }),
    review: reviewStatus(snapshot),
    activeStage: activeStage(snapshot),
    candidateId: snapshot.candidateId ?? null,
    interruption: snapshot.interruption === undefined
      ? null
      : Object.freeze({
        sequence: snapshot.interruption.sequence,
        resumeState: snapshot.interruption.resumeState,
        reason: snapshot.interruption.reason,
        stageRunId: snapshot.interruption.stageRunId ?? null,
      }),
    lastStop: snapshot.lastStop === undefined
      ? null
      : Object.freeze({
        kind: snapshot.lastStop.kind,
        occurredAtMillis: snapshot.lastStop.occurredAtMillis,
        message: snapshot.lastStop.message,
        code: snapshot.lastStop.code ?? null,
        retryable: snapshot.lastStop.retryable ?? null,
        stage: snapshot.lastStop.stage ?? null,
        stageRunId: snapshot.lastStop.stageRunId ?? null,
      }),
    executionLock: executionLock(snapshot),
    allowedOperations: allowedOperations(snapshot),
  })
}

function eventMessage(event: StrongFlowJobEvent): string {
  switch (event.kind) {
    case 'job.created': return 'StrongFlow 作业已创建。'
    case 'stage.started': return `${event.data.stage} 阶段已开始。`
    case 'stage.succeeded': return `${event.data.stage} 阶段已完成。`
    case 'stage.failed': return `${event.data.stage} 阶段失败：${event.data.message}`
    case 'human-review.approved': return '人工已批准当前定义。'
    case 'human-review.changes-requested': return `人工要求修改 ${event.data.scope}。`
    case 'human-review.rejected': return '人工已拒绝当前定义。'
    case 'job.interrupted': return `作业已中断：${event.data.reason}`
    case 'job.resumed': return '作业已从准确的中断点恢复。'
    case 'job.cancelled': return `作业已取消：${event.data.reason}`
    case 'completion-gate.passed': return '完成门禁已通过。'
    case 'completion-gate.failed': return `完成门禁未通过：${event.data.reason}`
    case 'job.delivered': return '作业已经交付。'
  }
}

function eventDefinition(
  event: StrongFlowJobEvent,
  snapshot: StrongFlowJobSnapshot,
): DefinitionIdentity | null {
  if (event.kind === 'human-review.approved'
    || event.kind === 'human-review.changes-requested'
    || event.kind === 'human-review.rejected') return event.data.definition
  return completeDefinition(snapshot)
}

function eventCandidateId(
  event: StrongFlowJobEvent,
  snapshot: StrongFlowJobSnapshot,
): StrongFlowOperatorEventView['candidateId'] {
  if (event.kind === 'stage.succeeded'
    && 'candidateId' in event.data) return event.data.candidateId
  if (event.kind === 'completion-gate.passed'
    || event.kind === 'completion-gate.failed'
    || event.kind === 'job.delivered') return event.data.candidateId
  return snapshot.candidateId ?? null
}

function eventArtifactLinks(
  event: StrongFlowJobEvent,
  records: readonly StrongFlowArtifactStoreArtifactRecord[],
): readonly StrongFlowOperatorArtifactLink[] {
  const selected = records.filter(record => {
    if (event.kind === 'job.created') return record.identity.artifactKind === 'USER_REQUEST'
    if (event.kind === 'human-review.approved'
      || event.kind === 'human-review.changes-requested'
      || event.kind === 'human-review.rejected') {
      return record.identity.artifactKind === 'HUMAN_REVIEW_RECORD'
        && record.identity.artifactId === event.data.reviewId
    }
    if (event.kind === 'stage.succeeded') {
      return record.producer.kind === 'role'
        && record.producer.stageRunId === event.data.stageRunId
    }
    return false
  })
  return Object.freeze(selected.map(artifactLink))
}

function eventView(
  event: StrongFlowJobEvent,
  snapshot: StrongFlowJobSnapshot,
  events: readonly StrongFlowJobEvent[],
  records: readonly StrongFlowArtifactStoreArtifactRecord[],
): StrongFlowOperatorEventView {
  const stage = eventStage(event, events)
  return Object.freeze({
    schemaVersion: STRONGFLOW_OPERATOR_EVENT_SCHEMA_VERSION,
    eventId: event.id,
    cursor: strongFlowOperatorEventCursor(event.jobId, event.sequence, event.id),
    jobId: event.jobId,
    sequence: event.sequence,
    occurredAtMillis: event.occurredAtMillis,
    kind: event.kind,
    state: snapshot.state,
    source: operatorEventSource(event, stage),
    stage,
    candidateId: eventCandidateId(event, snapshot),
    definition: eventDefinition(event, snapshot),
    artifactLinks: eventArtifactLinks(event, records),
    change: null,
    message: eventMessage(event),
  })
}

function projectEventViews(
  events: readonly StrongFlowJobEvent[],
  records: readonly StrongFlowArtifactStoreArtifactRecord[],
): readonly StrongFlowOperatorEventView[] {
  let snapshot: StrongFlowJobSnapshot | undefined
  const views: StrongFlowOperatorEventView[] = []
  for (const event of events) {
    snapshot = applyStrongFlowJobEvent(snapshot, event)
    views.push(eventView(event, snapshot, events, records))
  }
  return Object.freeze(views)
}

function delay(millis: number, signal?: AbortSignal): Promise<void> {
  if (signal?.aborted === true) {
    return Promise.reject(new OperatorFault('OPERATION_ABORTED', '操作请求已中止。'))
  }
  return new Promise((resolvePromise, rejectPromise) => {
    let timer: ReturnType<typeof setTimeout>
    const abort = (): void => {
      clearTimeout(timer)
      signal?.removeEventListener('abort', abort)
      rejectPromise(new OperatorFault('OPERATION_ABORTED', '操作请求已中止。'))
    }
    const finish = (): void => {
      signal?.removeEventListener('abort', abort)
      resolvePromise()
    }
    timer = setTimeout(finish, millis)
    if (signal === undefined) return
    signal.addEventListener('abort', abort, { once: true })
  })
}

/**
 * Durable, transport-neutral StrongFlow operator module. UI and CLI call the
 * same invoke seam; only an explicit cancel operation changes job ownership.
 */
export class StrongFlowLocalJobService implements StrongFlowOperatorInvoker {
  readonly home: string
  readonly #authenticator: HumanReviewAuthenticator
  readonly #scheduler: StrongFlowOperatorJobScheduler | undefined
  readonly #clock: () => number
  readonly #followPollMillis: number
  #mutationTail: Promise<void> = Promise.resolve()
  #recovery: Promise<void> | undefined

  constructor(options: StrongFlowOperatorServiceOptions) {
    if (!isRecord(options)
      || typeof options.home !== 'string'
      || options.home.length === 0
      || typeof options.authenticator?.authenticate !== 'function') {
      throw new StrongFlowOperatorServiceError(
        'INVALID_SERVICE_OPTIONS',
        'StrongFlow operator service requires a home and human authenticator',
      )
    }
    if (options.scheduler !== undefined
      && (typeof options.scheduler.jobReady !== 'function'
        || (options.scheduler.jobCancelled !== undefined
          && typeof options.scheduler.jobCancelled !== 'function'))) {
      throw new StrongFlowOperatorServiceError(
        'INVALID_SERVICE_OPTIONS',
        'StrongFlow operator scheduler is invalid',
      )
    }
    if (options.clock !== undefined && typeof options.clock !== 'function') {
      throw new StrongFlowOperatorServiceError(
        'INVALID_SERVICE_OPTIONS',
        'StrongFlow operator clock must be a function',
      )
    }
    const followPollMillis = options.followPollMillis ?? 25
    if (!Number.isSafeInteger(followPollMillis)
      || followPollMillis < 1
      || followPollMillis > 1_000) {
      throw new StrongFlowOperatorServiceError(
        'INVALID_SERVICE_OPTIONS',
        'StrongFlow follow poll interval must be between 1 and 1000 milliseconds',
      )
    }
    this.home = resolve(options.home)
    this.#authenticator = options.authenticator
    this.#scheduler = options.scheduler
    this.#clock = options.clock ?? Date.now
    this.#followPollMillis = followPollMillis
  }

  async invoke<Operation extends StrongFlowOperatorOperation>(
    requestValue: StrongFlowOperatorRequestFor<Operation>,
    options: { readonly signal?: AbortSignal } = {},
  ): Promise<StrongFlowOperatorResponseFor<Operation>> {
    const request = parseStrongFlowOperatorRequest(
      requestValue,
    ) as StrongFlowOperatorRequestFor<Operation>
    try {
      await this.#ensureRecovered()
    } catch (error) {
      return this.#failureResponse(request, error)
    }
    if (!MUTATING_OPERATIONS.has(request.operation)) {
      return this.#invokeAndValidate(request, options)
    }
    return this.#serializeMutation(async () => {
      try {
        const cached = await this.#readReceipt(request)
        if (cached !== undefined) return cached
        await this.#claimRequest(request)
        const response = await this.#invokeAndValidate(request, options)
        if (response.ok || !response.error.retryable) await this.#writeReceipt(request, response)
        return response
      } catch (error) {
        return this.#failureResponse(request, error)
      }
    })
  }

  async #failureResponse<Operation extends StrongFlowOperatorOperation>(
    request: StrongFlowOperatorRequestFor<Operation>,
    error: unknown,
  ): Promise<StrongFlowOperatorResponseFor<Operation>> {
    const fault = await this.#publicFault(
      request as unknown as StrongFlowOperatorRequest,
      error,
    )
    return parseStrongFlowOperatorResponseForRequest(request, materializeStrongFlowOperatorFailure({
      requestId: request.requestId,
      operation: request.operation,
      code: fault.code,
      message: fault.message,
      field: fault.field,
      currentDefinition: fault.currentDefinition,
    }))
  }

  async #invokeAndValidate<Operation extends StrongFlowOperatorOperation>(
    request: StrongFlowOperatorRequestFor<Operation>,
    options: { readonly signal?: AbortSignal },
  ): Promise<StrongFlowOperatorResponseFor<Operation>> {
    let response: StrongFlowOperatorResponse
    try {
      if (options.signal?.aborted === true) {
        throw new OperatorFault('OPERATION_ABORTED', '操作请求已中止。')
      }
      response = await this.#execute(
        request as unknown as StrongFlowOperatorRequest,
        options.signal,
      )
    } catch (error) {
      return this.#failureResponse(request, error)
    }
    return parseStrongFlowOperatorResponseForRequest(request, response)
  }

  async #execute(
    request: StrongFlowOperatorRequest,
    signal?: AbortSignal,
  ): Promise<StrongFlowOperatorResponse> {
    switch (request.operation) {
      case 'job.create': return this.#createJob(request)
      case 'job.status': {
        const stored = await this.#readJob(request.payload.jobId)
        return materializeStrongFlowOperatorSuccess(request, {
          job: jobView(stored.job.snapshot),
        })
      }
      case 'job.follow': return this.#followJob(request, signal)
      case 'definition.requirement': return this.#readDefinitionArtifact(
        request,
        'REQUIREMENT_SPEC',
      )
      case 'definition.solution': return this.#readDefinitionArtifact(
        request,
        'SOLUTION_DESIGN',
      )
      case 'definition.diagrams': return this.#readDiagrams(request)
      case 'review.approve': return this.#submitReview(request, 'approved')
      case 'review.reject': return this.#submitReview(request, 'rejected')
      case 'review.request-changes': return this.#submitReview(request, 'changes-requested')
      case 'job.cancel': return this.#cancelJob(request)
      case 'job.resume': return this.#resumeJob(request)
      case 'job.artifacts': return this.#listArtifacts(request)
      case 'job.export': return this.#exportJob(request)
    }
  }

  async #createJob(
    request: Extract<StrongFlowOperatorRequest, { readonly operation: 'job.create' }>,
  ): Promise<StrongFlowOperatorResponse> {
    if (containsStrongFlowCredentialMaterial(request.payload)) {
      throw new OperatorFault(
        'INVALID_REQUEST',
        '创建请求包含原始凭据内容，尚未建立作业。',
        { field: 'request.payload' },
      )
    }
    const configuration = await this.#ensureJobConfiguration(request)
    const actorId = requestActorId(request.requestId)
    const createdEvent = createStrongFlowJobEvent({
      jobId: configuration.jobId,
      sequence: '1',
      occurredAtMillis: configuration.createdAtMillis,
      source: { kind: 'system', actorId },
      kind: 'job.created',
      data: configuration.title === null ? {} : { title: configuration.title },
    })
    let jobStore: StrongFlowJobStore
    try {
      jobStore = await StrongFlowJobStore.create({ home: this.home, event: createdEvent })
    } catch (error) {
      if (!(error instanceof StrongFlowJobStoreError) || error.code !== 'JOB_ALREADY_EXISTS') {
        throw error
      }
      jobStore = await StrongFlowJobStore.open(this.home, configuration.jobId)
      const stored = await jobStore.read()
      if (!isDeepStrictEqual(stored.events[0], createdEvent)) {
        throw new OperatorFault('JOB_CONFLICT', '请求身份已经对应另一个作业。')
      }
    }
    let artifactStore: StrongFlowArtifactStore
    try {
      artifactStore = await StrongFlowArtifactStore.create({
        home: this.home,
        jobId: configuration.jobId,
        createdAtMillis: configuration.createdAtMillis,
      })
    } catch (error) {
      if (!(error instanceof StrongFlowArtifactStoreError)
        || error.code !== 'JOB_ALREADY_EXISTS') throw error
      artifactStore = await StrongFlowArtifactStore.open(this.home, configuration.jobId)
    }
    const userRequest = materializeStrongFlowArtifact('USER_REQUEST', {
      artifactId: UserRequestId(configuration.userRequestArtifactId),
      jobId: configuration.jobId,
      sourceArtifacts: [],
      producer: { kind: 'system', actorId },
      kernelEventInterval: null,
      createdAtMillis: configuration.createdAtMillis,
    }, {
      request: request.payload.request,
      submittedFrom: request.payload.submittedFrom === 'local-ui'
        ? 'strongflow-workbench'
        : 'cli',
    })
    await artifactStore.publishArtifact(userRequest)
    const initialSnapshot = projectStrongFlowJob([createdEvent])
    this.#notifyRunnable({ configuration, jobStore, artifactStore })
    return materializeStrongFlowOperatorSuccess(request, { job: jobView(initialSnapshot) })
  }

  async #followJob(
    request: Extract<StrongFlowOperatorRequest, { readonly operation: 'job.follow' }>,
    signal?: AbortSignal,
  ): Promise<StrongFlowOperatorResponse> {
    const startedAt = Date.now()
    while (true) {
      if (signal?.aborted === true) {
        throw new OperatorFault('OPERATION_ABORTED', '事件等待已中止。')
      }
      const stored = await this.#readJob(request.payload.jobId)
      const after = request.payload.afterCursor === null
        ? null
        : parseStrongFlowOperatorEventCursor(request.payload.afterCursor)
      const afterSequence = after?.sequence ?? '0'
      if (BigInt(afterSequence) > BigInt(stored.job.snapshot.sequence)) {
        throw new OperatorFault(
          'INVALID_CURSOR',
          '事件游标晚于当前作业记录。',
          { field: 'request.payload.afterCursor' },
        )
      }
      if (after !== null) {
        const anchor = stored.job.events.find(event => event.sequence === after.sequence)
        if (anchor?.id !== after.eventId) {
          throw new OperatorFault(
            'INVALID_CURSOR',
            '事件游标不对应这个作业中的正式事件。',
            { field: 'request.payload.afterCursor' },
          )
        }
      }
      const remaining = stored.job.events.filter(event => (
        BigInt(event.sequence) > BigInt(afterSequence)
      ))
      if (remaining.length > 0 || request.payload.waitMillis === 0) {
        return materializeStrongFlowOperatorSuccess(
          request,
          await this.#eventPage(request.payload.afterCursor, request.payload.limit, stored),
        )
      }
      const elapsed = Date.now() - startedAt
      if (elapsed >= request.payload.waitMillis) {
        return materializeStrongFlowOperatorSuccess(
          request,
          await this.#eventPage(request.payload.afterCursor, request.payload.limit, stored),
        )
      }
      await delay(
        Math.min(this.#followPollMillis, request.payload.waitMillis - elapsed),
        signal,
      )
    }
  }

  async #eventPage(
    afterCursor: StrongFlowOperatorEventPage['afterCursor'],
    limit: number,
    stored: OpenedOperatorJob,
  ): Promise<StrongFlowOperatorEventPage> {
    const records = await this.#artifactRecords(stored.artifacts)
    const views = projectEventViews(stored.job.events, records)
    const afterSequence = afterCursor === null
      ? 0n
      : BigInt(parseStrongFlowOperatorEventCursor(afterCursor).sequence)
    const remaining = views.filter(event => BigInt(event.sequence) > afterSequence)
    const events = Object.freeze(remaining.slice(0, limit))
    return Object.freeze({
      schemaVersion: STRONGFLOW_OPERATOR_EVENT_SCHEMA_VERSION,
      jobId: stored.job.snapshot.jobId,
      afterCursor,
      events,
      nextCursor: events.at(-1)?.cursor ?? afterCursor,
      caughtUp: remaining.length <= events.length,
    })
  }

  async #readDefinitionArtifact(
    request: Extract<StrongFlowOperatorRequest, {
      readonly operation: 'definition.requirement' | 'definition.solution'
    }>,
    kind: 'REQUIREMENT_SPEC' | 'SOLUTION_DESIGN',
  ): Promise<StrongFlowOperatorResponse> {
    const stored = await this.#readJob(request.payload.jobId)
    const artifactId = kind === 'REQUIREMENT_SPEC'
      ? stored.job.snapshot.definition.requirementId
      : stored.job.snapshot.definition.solutionId
    if (artifactId === undefined) {
      throw new OperatorFault('WRONG_JOB_STATE', '当前作业还没有生成所请求的定义制品。')
    }
    const found = await stored.artifacts.findArtifact(kind, artifactId)
    if (found === undefined) {
      throw new OperatorFault('ARTIFACT_NOT_FOUND', '当前定义制品正文不存在。')
    }
    if (request.operation === 'definition.requirement') {
      return materializeStrongFlowOperatorSuccess(request, {
        job: jobView(stored.job.snapshot),
        link: artifactLink(found.record),
        artifact: found.artifact as Extract<StrongFlowArtifact, {
          readonly artifactKind: 'REQUIREMENT_SPEC'
        }>,
      })
    }
    return materializeStrongFlowOperatorSuccess(request, {
      job: jobView(stored.job.snapshot),
      link: artifactLink(found.record),
      artifact: found.artifact as Extract<StrongFlowArtifact, {
        readonly artifactKind: 'SOLUTION_DESIGN'
      }>,
    })
  }

  async #readDiagrams(
    request: Extract<StrongFlowOperatorRequest, { readonly operation: 'definition.diagrams' }>,
  ): Promise<StrongFlowOperatorResponse> {
    const stored = await this.#readJob(request.payload.jobId)
    const definition = completeDefinition(stored.job.snapshot)
    if (definition === null) {
      throw new OperatorFault('WRONG_JOB_STATE', '当前作业还没有完整的两张定义图。')
    }
    const [systemArchitecture, processFlow] = await Promise.all([
      stored.artifacts.findArtifact(
        'SYSTEM_ARCHITECTURE_DIAGRAM',
        definition.systemArchitectureDiagramId,
      ),
      stored.artifacts.findArtifact(
        'PROCESS_FLOW_DIAGRAM',
        definition.processFlowDiagramId,
      ),
    ])
    if (systemArchitecture === undefined || processFlow === undefined) {
      throw new OperatorFault('ARTIFACT_NOT_FOUND', '当前定义图正文不存在。')
    }
    return materializeStrongFlowOperatorSuccess(request, {
      job: jobView(stored.job.snapshot),
      definition,
      systemArchitecture: {
        link: artifactLink(systemArchitecture.record),
        artifact: systemArchitecture.artifact as Extract<StrongFlowArtifact, {
          readonly artifactKind: 'SYSTEM_ARCHITECTURE_DIAGRAM'
        }>,
      },
      processFlow: {
        link: artifactLink(processFlow.record),
        artifact: processFlow.artifact as Extract<StrongFlowArtifact, {
          readonly artifactKind: 'PROCESS_FLOW_DIAGRAM'
        }>,
      },
    })
  }

  async #submitReview(
    request: Extract<StrongFlowOperatorRequest, {
      readonly operation: 'review.approve' | 'review.reject' | 'review.request-changes'
    }>,
    decision: 'approved' | 'rejected' | 'changes-requested',
  ): Promise<StrongFlowOperatorResponse> {
    const stored = await this.#readJob(request.payload.jobId)
    const reviewId = deterministicReviewId(request.requestId)
    let receipt = recoverHumanReviewReceipt(
      stored.job.events,
      reviewId,
      decision,
      request.payload.definition,
    )
    if (receipt === undefined) {
      const gate = new StrongFlowHumanReviewGate({
        store: stored.jobStore,
        authenticator: this.#authenticator,
        clock: this.#clock,
        reviewIdFactory: () => reviewId,
      })
      try {
        receipt = await gate.submit({
          decision,
          channel: request.payload.channel,
          authentication: request.payload.authentication,
          definition: request.payload.definition,
          ...(request.payload.comment === null ? {} : { comment: request.payload.comment }),
          ...(request.operation === 'review.request-changes'
            ? { scope: request.payload.scope }
            : {}),
        })
      } catch (error) {
        if (!(error instanceof HumanReviewGateError)
          || error.code !== 'REVIEW_ALREADY_DECIDED') throw error
        const raced = await stored.jobStore.read()
        receipt = recoverHumanReviewReceipt(
          raced.events,
          reviewId,
          decision,
          request.payload.definition,
        )
        if (receipt === undefined) throw error
      }
    }
    const publication = await stored.artifacts.publishArtifact(receipt.decision)
    const latest = await stored.jobStore.read()
    const eventRecords = publication.record.entryKind === 'artifact'
      ? [publication.record]
      : []
    const event = eventView(receipt.event, receipt.snapshot, latest.events, eventRecords)
    if (receipt.snapshot.state !== 'REJECTED') {
      await this.#notifyJob(request.payload.jobId)
    }
    return materializeStrongFlowOperatorSuccess(request, {
      job: jobView(receipt.snapshot),
      event,
      review: receipt.decision,
    })
  }

  async #cancelJob(
    request: Extract<StrongFlowOperatorRequest, { readonly operation: 'job.cancel' }>,
  ): Promise<StrongFlowOperatorResponse> {
    const stored = await this.#readJob(request.payload.jobId)
    const actorId = requestActorId(request.requestId)
    const recovered = stored.job.events.find(event => (
      event.kind === 'job.cancelled'
      && event.source.kind === 'system'
      && event.source.actorId === actorId
    ))
    let event: Extract<StrongFlowJobEvent, { readonly kind: 'job.cancelled' }>
    let snapshot: StrongFlowJobSnapshot
    if (recovered?.kind === 'job.cancelled') {
      if (recovered.data.reason !== request.payload.reason) {
        throw new OperatorFault('JOB_CONFLICT', '取消请求身份对应另一个取消原因。')
      }
      event = recovered
      const index = stored.job.events.indexOf(recovered)
      snapshot = projectStrongFlowJob(stored.job.events.slice(0, index + 1))
    } else {
      if (TERMINAL_STATES.has(stored.job.snapshot.state)) {
        throw new OperatorFault('JOB_TERMINAL', '终态作业不能再次取消。')
      }
      event = createStrongFlowJobEvent({
        jobId: request.payload.jobId,
        sequence: nextSequence(stored.job.snapshot),
        occurredAtMillis: Math.max(
          validateClock(this.#clock),
          stored.job.snapshot.lastOccurredAtMillis,
        ),
        source: { kind: 'system', actorId },
        kind: 'job.cancelled',
        data: { reason: request.payload.reason },
      })
      try {
        snapshot = await stored.jobStore.append(event)
      } catch (error) {
        if (!(error instanceof StrongFlowJobStoreError)
          || (error.code !== 'EVENT_ALREADY_EXISTS'
            && error.code !== 'EVENT_SEQUENCE_MISMATCH')) throw error
        const raced = await stored.jobStore.read()
        const racedEvent = raced.events.find(candidate => (
          candidate.kind === 'job.cancelled'
          && candidate.source.kind === 'system'
          && candidate.source.actorId === actorId
        ))
        if (racedEvent?.kind !== 'job.cancelled') throw error
        if (racedEvent.data.reason !== request.payload.reason) {
          throw new OperatorFault('JOB_CONFLICT', '取消请求身份对应另一个取消原因。')
        }
        event = racedEvent
        const index = raced.events.indexOf(racedEvent)
        snapshot = projectStrongFlowJob(raced.events.slice(0, index + 1))
      }
    }
    this.#notifyCancelled(request.payload.jobId, request.payload.reason)
    const latest = await stored.jobStore.read()
    return materializeStrongFlowOperatorSuccess(request, {
      job: jobView(snapshot),
      event: eventView(event, snapshot, latest.events, []),
      review: null,
    })
  }

  async #resumeJob(
    request: Extract<StrongFlowOperatorRequest, { readonly operation: 'job.resume' }>,
  ): Promise<StrongFlowOperatorResponse> {
    const stored = await this.#readJob(request.payload.jobId)
    const actorId = requestActorId(request.requestId)
    const recovered = stored.job.events.find(event => (
      event.kind === 'job.resumed'
      && event.source.kind === 'system'
      && event.source.actorId === actorId
    ))
    let event: Extract<StrongFlowJobEvent, { readonly kind: 'job.resumed' }>
    let snapshot: StrongFlowJobSnapshot
    if (recovered?.kind === 'job.resumed') {
      if (recovered.data.interruptionSequence !== request.payload.interruptionSequence) {
        throw new OperatorFault('JOB_CONFLICT', '恢复请求身份对应另一个中断点。')
      }
      event = recovered
      const index = stored.job.events.indexOf(recovered)
      snapshot = projectStrongFlowJob(stored.job.events.slice(0, index + 1))
    } else {
      if (stored.job.snapshot.state !== 'INTERRUPTED'
        || stored.job.snapshot.interruption === undefined) {
        throw new OperatorFault('WRONG_JOB_STATE', '只有已中断的作业可以恢复。')
      }
      if (stored.job.snapshot.interruption.sequence
        !== request.payload.interruptionSequence) {
        throw new OperatorFault('JOB_CONFLICT', '提交的中断序号不是当前中断点。')
      }
      event = createStrongFlowJobEvent({
        jobId: request.payload.jobId,
        sequence: nextSequence(stored.job.snapshot),
        occurredAtMillis: Math.max(
          validateClock(this.#clock),
          stored.job.snapshot.lastOccurredAtMillis,
        ),
        source: { kind: 'system', actorId },
        kind: 'job.resumed',
        data: { interruptionSequence: request.payload.interruptionSequence },
      })
      try {
        snapshot = await stored.jobStore.append(event)
      } catch (error) {
        if (!(error instanceof StrongFlowJobStoreError)
          || (error.code !== 'EVENT_ALREADY_EXISTS'
            && error.code !== 'EVENT_SEQUENCE_MISMATCH')) throw error
        const raced = await stored.jobStore.read()
        const racedEvent = raced.events.find(candidate => (
          candidate.kind === 'job.resumed'
          && candidate.source.kind === 'system'
          && candidate.source.actorId === actorId
        ))
        if (racedEvent?.kind !== 'job.resumed') throw error
        if (racedEvent.data.interruptionSequence
          !== request.payload.interruptionSequence) {
          throw new OperatorFault('JOB_CONFLICT', '恢复请求身份对应另一个中断点。')
        }
        event = racedEvent
        const index = raced.events.indexOf(racedEvent)
        snapshot = projectStrongFlowJob(raced.events.slice(0, index + 1))
      }
    }
    const latest = await stored.jobStore.read()
    await this.#notifyJob(request.payload.jobId)
    return materializeStrongFlowOperatorSuccess(request, {
      job: jobView(snapshot),
      event: eventView(event, snapshot, latest.events, []),
      review: null,
    })
  }

  async #listArtifacts(
    request: Extract<StrongFlowOperatorRequest, { readonly operation: 'job.artifacts' }>,
  ): Promise<StrongFlowOperatorResponse> {
    const stored = await this.#readJob(request.payload.jobId)
    const records = (await this.#artifactRecords(stored.artifacts)).filter(record => (
      (request.payload.afterSequence === null
        || BigInt(record.sequence) > BigInt(request.payload.afterSequence))
      && request.payload.artifactKinds.includes(record.identity.artifactKind)
    ))
    const selected = records.slice(0, request.payload.limit).map(artifactLink)
    return materializeStrongFlowOperatorSuccess(request, {
      schemaVersion: STRONGFLOW_OPERATOR_ARTIFACT_LINK_SCHEMA_VERSION,
      jobId: request.payload.jobId,
      afterSequence: request.payload.afterSequence,
      artifacts: Object.freeze(selected),
      nextAfterSequence: selected.at(-1)?.sequence ?? null,
    })
  }

  async #exportJob(
    request: Extract<StrongFlowOperatorRequest, { readonly operation: 'job.export' }>,
  ): Promise<StrongFlowOperatorResponse> {
    const stored = await this.#readJob(request.payload.jobId)
    if (stored.job.events.length > STRONGFLOW_OPERATOR_MAX_EXPORT_EVENTS) {
      throw new OperatorFault('LIMIT_EXCEEDED', '作业事件数量超过单次导出上限。')
    }
    const records = await this.#artifactRecords(stored.artifacts)
    if (records.length > STRONGFLOW_OPERATOR_MAX_EXPORT_ARTIFACTS) {
      throw new OperatorFault('LIMIT_EXCEEDED', '作业制品数量超过单次导出上限。')
    }
    return materializeStrongFlowOperatorSuccess(request, {
      schemaVersion: STRONGFLOW_OPERATOR_EXPORT_SCHEMA_VERSION,
      format: 'manifest-json',
      exportedAtMillis: Math.max(
        validateClock(this.#clock),
        stored.job.snapshot.lastOccurredAtMillis,
      ),
      job: jobView(stored.job.snapshot),
      events: projectEventViews(stored.job.events, records),
      artifacts: Object.freeze(records.map(artifactLink)),
    })
  }

  async #readJob(jobId: JobIdentifier): Promise<OpenedOperatorJob> {
    const [jobStore, artifacts] = await Promise.all([
      StrongFlowJobStore.open(this.home, jobId),
      StrongFlowArtifactStore.open(this.home, jobId),
    ])
    const job = await jobStore.read()
    return Object.freeze({ jobStore, artifacts, job })
  }

  async #artifactRecords(
    store: StrongFlowArtifactStore,
  ): Promise<readonly StrongFlowArtifactStoreArtifactRecord[]> {
    const records: StrongFlowArtifactStoreArtifactRecord[] = []
    let afterSequence: string | undefined
    while (true) {
      const page = await store.list({
        limit: 1_000,
        entryKinds: ['artifact'],
        ...(afterSequence === undefined ? {} : { afterSequence }),
      })
      records.push(...page.records.filter(
        (record): record is StrongFlowArtifactStoreArtifactRecord => (
          record.entryKind === 'artifact'
        ),
      ))
      if (page.nextAfterSequence === null) break
      afterSequence = page.nextAfterSequence
    }
    return Object.freeze(records)
  }

  async #ensureJobConfiguration(
    request: Extract<StrongFlowOperatorRequest, { readonly operation: 'job.create' }>,
  ): Promise<StrongFlowOperatorJobConfiguration> {
    const requestHash = requestSha256(request)
    const path = jobConfigurationPath(this.home, deterministicJobId(request.requestId))
    const existing = await readJson(path)
    if (existing !== undefined) {
      const parsed = parseJobConfiguration(existing)
      if (parsed.requestId !== request.requestId || parsed.requestSha256 !== requestHash) {
        throw new OperatorFault('JOB_CONFLICT', '请求身份已经对应不同的创建内容。')
      }
      return parsed
    }
    const configuration = Object.freeze({
      schemaVersion: STRONGFLOW_OPERATOR_SERVICE_SCHEMA_VERSION,
      jobId: deterministicJobId(request.requestId),
      requestId: request.requestId,
      requestSha256: requestHash,
      repositoryPath: resolve(request.payload.repositoryPath),
      baseRevision: request.payload.baseRevision,
      title: request.payload.title,
      userRequestArtifactId: deterministicUserRequestId(request.requestId),
      createdAtMillis: validateClock(this.#clock),
    })
    const written = await writeExclusiveJson(path, configuration)
    if (written) return configuration
    const raced = await readJson(path)
    if (raced === undefined) throw new Error('job configuration publication disappeared')
    const parsed = parseJobConfiguration(raced)
    if (parsed.requestId !== request.requestId || parsed.requestSha256 !== requestHash) {
      throw new OperatorFault('JOB_CONFLICT', '请求身份已经对应不同的创建内容。')
    }
    return parsed
  }

  async #readJobConfiguration(
    jobId: JobIdentifier,
  ): Promise<StrongFlowOperatorJobConfiguration | undefined> {
    const value = await readJson(jobConfigurationPath(this.home, jobId))
    return value === undefined ? undefined : parseJobConfiguration(value)
  }

  async #readReceipt<Operation extends StrongFlowOperatorOperation>(
    request: StrongFlowOperatorRequestFor<Operation>,
  ): Promise<StrongFlowOperatorResponseFor<Operation> | undefined> {
    let value: unknown | undefined
    try {
      value = await readJson(requestReceiptPath(this.home, request.requestId))
    } catch (error) {
      throw new OperatorFault('STORE_FAILURE', '幂等请求记录读取失败。', { cause: error })
    }
    if (value === undefined) return undefined
    let receipt: StoredRequestReceipt
    try {
      receipt = parseStoredRequestReceipt(value)
    } catch (error) {
      throw new OperatorFault('STORE_FAILURE', '幂等请求记录已经损坏。', { cause: error })
    }
    if (receipt.requestId !== request.requestId
      || receipt.operation !== request.operation
      || receipt.requestSha256 !== requestSha256(request)) {
      throw new OperatorFault('JOB_CONFLICT', '请求身份已经用于另一个变更操作。')
    }
    return parseStrongFlowOperatorResponseForRequest(request, receipt.response)
  }

  async #claimRequest<Operation extends StrongFlowOperatorOperation>(
    request: StrongFlowOperatorRequestFor<Operation>,
  ): Promise<void> {
    const claim = Object.freeze({
      schemaVersion: STRONGFLOW_OPERATOR_SERVICE_SCHEMA_VERSION,
      requestId: request.requestId,
      operation: request.operation,
      requestSha256: requestSha256(request),
    })
    const path = requestClaimPath(this.home, request.requestId)
    try {
      if (await writeExclusiveJson(path, claim)) return
      const existing = await readJson(path)
      if (existing === undefined) throw new Error('request claim disappeared')
      const parsed = parseStoredRequestClaim(existing)
      if (parsed.requestId !== claim.requestId
        || parsed.operation !== claim.operation
        || parsed.requestSha256 !== claim.requestSha256) {
        throw new OperatorFault('JOB_CONFLICT', '请求身份已经用于另一个变更操作。')
      }
    } catch (error) {
      if (error instanceof OperatorFault) throw error
      throw new OperatorFault('STORE_FAILURE', '幂等请求声明写入失败。', { cause: error })
    }
  }

  async #writeReceipt<Operation extends StrongFlowOperatorOperation>(
    request: StrongFlowOperatorRequestFor<Operation>,
    response: StrongFlowOperatorResponseFor<Operation>,
  ): Promise<void> {
    const receipt = Object.freeze({
      schemaVersion: STRONGFLOW_OPERATOR_SERVICE_SCHEMA_VERSION,
      requestId: request.requestId,
      operation: request.operation,
      requestSha256: requestSha256(request),
      response,
    })
    try {
      const written = await writeExclusiveJson(
        requestReceiptPath(this.home, request.requestId),
        receipt,
      )
      if (written) return
      const existing = await this.#readReceipt(request)
      if (existing === undefined || !isDeepStrictEqual(existing, response)) {
        throw new OperatorFault('JOB_CONFLICT', '同一请求身份产生了不同结果。')
      }
    } catch (error) {
      if (error instanceof OperatorFault) throw error
      throw new OperatorFault('STORE_FAILURE', '幂等请求记录写入失败。', { cause: error })
    }
  }

  async #publicFault(
    request: StrongFlowOperatorRequest,
    error: unknown,
  ): Promise<OperatorFault> {
    if (error instanceof OperatorFault) return error
    if (error instanceof HumanReviewGateError) {
      if (error.code === 'AUTHENTICATION_REQUIRED') {
        return new OperatorFault('AUTHENTICATION_REQUIRED', '人工审核认证未通过。')
      }
      if (error.code === 'AUTHENTICATION_FAILED') {
        return new OperatorFault('AUTHENTICATION_FAILED', '人工审核认证失败。')
      }
      if (error.code === 'STALE_DEFINITION') {
        const currentDefinition = await this.#currentDefinition(request)
        if (currentDefinition !== null) {
          return new OperatorFault(
            'STALE_DEFINITION',
            '提交的定义身份不是当前版本。',
            { currentDefinition },
          )
        }
        return new OperatorFault('WRONG_JOB_STATE', '当前作业没有完整定义可供审核。')
      }
      if (error.code === 'REVIEW_ALREADY_DECIDED') {
        return new OperatorFault('REVIEW_ALREADY_DECIDED', '当前定义已经有人工决定。')
      }
      return new OperatorFault('WRONG_JOB_STATE', '当前作业不接受这个人工审核操作。')
    }
    if (error instanceof StrongFlowJobStoreError) {
      if (error.code === 'JOB_NOT_FOUND') {
        return new OperatorFault('JOB_NOT_FOUND', 'StrongFlow 作业不存在。')
      }
      if (error.code === 'EVENT_ALREADY_EXISTS'
        || error.code === 'EVENT_SEQUENCE_MISMATCH') {
        return new OperatorFault('JOB_CONFLICT', '作业已被另一个操作先行更新。')
      }
      return new OperatorFault('STORE_FAILURE', 'StrongFlow 作业记录读取或写入失败。')
    }
    if (error instanceof StrongFlowArtifactStoreError) {
      if (error.code === 'CREDENTIAL_MATERIAL_DENIED') {
        return new OperatorFault(
          'INVALID_REQUEST',
          '提交内容包含原始凭据，未写入 StrongFlow 制品。',
        )
      }
      if (error.code === 'JOB_NOT_FOUND') {
        return new OperatorFault('JOB_NOT_FOUND', 'StrongFlow 作业制品不存在。')
      }
      if (error.code === 'RECORD_NOT_FOUND' || error.code === 'CONTENT_MISSING') {
        return new OperatorFault('ARTIFACT_NOT_FOUND', 'StrongFlow 制品正文不存在。')
      }
      if (error.code === 'IDENTITY_CONFLICT') {
        return new OperatorFault('JOB_CONFLICT', 'StrongFlow 制品身份已经被占用。')
      }
      return new OperatorFault('STORE_FAILURE', 'StrongFlow 制品记录读取或写入失败。')
    }
    if (error instanceof StrongFlowOperatorServiceError) {
      if (error.code === 'JOB_CONFLICT') {
        return new OperatorFault('JOB_CONFLICT', error.message)
      }
      if (error.code === 'WRONG_JOB_STATE') {
        return new OperatorFault('WRONG_JOB_STATE', error.message)
      }
      if (error.code === 'JOB_TERMINAL') {
        return new OperatorFault('JOB_TERMINAL', error.message)
      }
      return new OperatorFault('STORE_FAILURE', 'StrongFlow 本地操作服务失败。')
    }
    return new OperatorFault('INTERNAL_ERROR', 'StrongFlow 操作发生内部错误。')
  }

  async #currentDefinition(request: StrongFlowOperatorRequest): Promise<DefinitionIdentity | null> {
    if (request.operation === 'job.create') return null
    try {
      return completeDefinition((await this.#readJob(request.payload.jobId)).job.snapshot)
    } catch {
      return null
    }
  }

  #notifyRunnable(job: StrongFlowOperatorRunnableJob): void {
    if (this.#scheduler === undefined) return
    queueMicrotask(() => {
      void Promise.resolve(this.#scheduler?.jobReady(job)).catch(() => {})
    })
  }

  #notifyCancelled(jobId: JobIdentifier, reason: string): void {
    if (this.#scheduler?.jobCancelled === undefined) return
    queueMicrotask(() => {
      void Promise.resolve(this.#scheduler?.jobCancelled?.(jobId, reason)).catch(() => {})
    })
  }

  async #notifyJob(jobId: JobIdentifier): Promise<void> {
    if (this.#scheduler === undefined) return
    const configuration = await this.#readJobConfiguration(jobId)
    if (configuration === undefined) return
    const stored = await this.#readJob(jobId)
    this.#notifyRunnable({
      configuration,
      jobStore: stored.jobStore,
      artifactStore: stored.artifacts,
    })
  }

  async #ensureRecovered(): Promise<void> {
    if (this.#scheduler === undefined) return
    this.#recovery ??= this.#recoverRunnableJobs()
    return this.#recovery
  }

  async #recoverRunnableJobs(): Promise<void> {
    const jobs = await StrongFlowJobStore.list(this.home)
    for (const entry of jobs) {
      if (TERMINAL_STATES.has(entry.state)
        || entry.state === 'AWAITING_HUMAN_REVIEW'
        || entry.state === 'INTERRUPTED') continue
      const configuration = await this.#readJobConfiguration(entry.manifest.jobId)
      if (configuration === undefined) continue
      const stored = await this.#readJob(entry.manifest.jobId)
      if (stored.job.snapshot.activeStage !== undefined) continue
      this.#notifyRunnable({
        configuration,
        jobStore: stored.jobStore,
        artifactStore: stored.artifacts,
      })
    }
  }

  #serializeMutation<Result>(operation: () => Promise<Result>): Promise<Result> {
    const current = this.#mutationTail.then(operation, operation)
    this.#mutationTail = current.then(() => {}, () => {})
    return current
  }
}
