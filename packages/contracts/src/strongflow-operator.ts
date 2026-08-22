import {
  AttemptId,
  CandidateId,
  DiagramId,
  HumanReviewId,
  JobId,
  KernelSessionId,
  RequirementId,
  SolutionId,
  StageRunId,
  STRONGFLOW_JOB_STAGES,
  STRONGFLOW_JOB_STATES,
  type CandidateId as CandidateIdentifier,
  type DefinitionIdentity,
  type DefinitionRevisionScope,
  type HumanReviewChannel,
  type JobId as JobIdentifier,
  type StrongFlowJobStage,
  type StrongFlowJobState,
} from './strongflow-job.js'
import {
  STRONGFLOW_ARTIFACT_KINDS,
  parseStrongFlowArtifactAs,
  parseStrongFlowCandidateIdentity,
  type HumanReviewRecord,
  type ProcessFlowDiagram,
  type RequirementSpec,
  type SolutionDesign,
  type StrongFlowArtifact,
  type StrongFlowArtifactKind,
  type SystemArchitectureDiagram,
} from './strongflow-artifact.js'
import {
  STRONGFLOW_ROLE_IDS,
  type StrongFlowRoleId,
} from './strongflow-role.js'
import type { StrongFlowCandidateIdentity } from './strongflow-workspace.js'

export const STRONGFLOW_OPERATOR_SCHEMA_VERSION = 1 as const
export const STRONGFLOW_OPERATOR_JOB_VIEW_SCHEMA_VERSION = 1 as const
export const STRONGFLOW_OPERATOR_EVENT_SCHEMA_VERSION = 1 as const
export const STRONGFLOW_OPERATOR_ARTIFACT_LINK_SCHEMA_VERSION = 1 as const
export const STRONGFLOW_OPERATOR_EXPORT_SCHEMA_VERSION = 1 as const
export const STRONGFLOW_OPERATOR_MEDIA_TYPE =
  'application/vnd.winwincode.strongflow-operator+json; version=1'
export const STRONGFLOW_OPERATOR_EVENT_STREAM_MEDIA_TYPE = 'application/x-ndjson; charset=utf-8'

export const STRONGFLOW_OPERATOR_OPERATIONS = Object.freeze([
  'job.create',
  'job.status',
  'job.follow',
  'definition.requirement',
  'definition.solution',
  'definition.diagrams',
  'review.approve',
  'review.reject',
  'review.request-changes',
  'job.cancel',
  'job.resume',
  'job.artifacts',
  'job.export',
] as const)

export type StrongFlowOperatorOperation = typeof STRONGFLOW_OPERATOR_OPERATIONS[number]

export const STRONGFLOW_OPERATOR_MUTATING_OPERATIONS = Object.freeze([
  'job.create',
  'review.approve',
  'review.reject',
  'review.request-changes',
  'job.cancel',
  'job.resume',
] as const satisfies readonly StrongFlowOperatorOperation[])

export const STRONGFLOW_OPERATOR_MAX_EVENT_PAGE = 500
export const STRONGFLOW_OPERATOR_MAX_ARTIFACT_PAGE = 500
export const STRONGFLOW_OPERATOR_MAX_FOLLOW_WAIT_MILLIS = 30_000
export const STRONGFLOW_OPERATOR_MAX_EXPORT_EVENTS = 10_000
export const STRONGFLOW_OPERATOR_MAX_EXPORT_ARTIFACTS = 10_000

const PORTABLE_IDENTIFIER_PATTERN = /^[A-Za-z0-9][A-Za-z0-9._:/@-]{0,499}$/u
const DECIMAL_SEQUENCE_PATTERN = /^(?:0|[1-9][0-9]*)$/u
const SHA256_PATTERN = /^[0-9a-f]{64}$/u
const BLOB_ID_PATTERN = /^sha256-[0-9a-f]{64}$/u
const MEDIA_TYPE_PATTERN = /^[a-z0-9!#$&^_.+-]+\/[a-z0-9!#$&^_.+-]+(?:; charset=utf-8)?$/u

declare const strongFlowOperatorIdentifierBrand: unique symbol

type StrongFlowOperatorIdentifier<Name extends string> = string & {
  readonly [strongFlowOperatorIdentifierBrand]: Name
}

export type StrongFlowOperatorRequestId = StrongFlowOperatorIdentifier<
  'StrongFlowOperatorRequestId'
>
export type StrongFlowOperatorEventCursor = StrongFlowOperatorIdentifier<
  'StrongFlowOperatorEventCursor'
>

export type StrongFlowOperatorValidationErrorCode =
  | 'INVALID_REQUEST'
  | 'INVALID_RESPONSE'
  | 'UNSUPPORTED_SCHEMA_VERSION'
  | 'INVALID_IDENTITY'
  | 'INVALID_CURSOR'
  | 'LIMIT_EXCEEDED'
  | 'RELATIONSHIP_MISMATCH'

export class StrongFlowOperatorValidationError extends Error {
  readonly code: StrongFlowOperatorValidationErrorCode
  readonly path: string

  constructor(
    code: StrongFlowOperatorValidationErrorCode,
    path: string,
    message: string,
    options?: ErrorOptions,
  ) {
    super(message, options)
    this.name = 'StrongFlowOperatorValidationError'
    this.code = code
    this.path = path
  }
}

function operatorError(
  code: StrongFlowOperatorValidationErrorCode,
  path: string,
  message: string,
  options?: ErrorOptions,
): never {
  throw new StrongFlowOperatorValidationError(code, path, message, options)
}

function isRecord(value: unknown): value is Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return false
  const prototype = Object.getPrototypeOf(value)
  return prototype === Object.prototype || prototype === null
}

function record(value: unknown, path: string, code: StrongFlowOperatorValidationErrorCode): Record<string, unknown> {
  if (!isRecord(value)) operatorError(code, path, `${path} must be an object`)
  return value
}

function exactKeys(
  value: Record<string, unknown>,
  keys: readonly string[],
  path: string,
  code: StrongFlowOperatorValidationErrorCode,
): void {
  const expected = new Set(keys)
  if (
    Object.keys(value).length !== expected.size
    || keys.some(key => !Object.hasOwn(value, key))
    || Object.keys(value).some(key => !expected.has(key))
  ) operatorError(code, path, `${path} has an unexpected shape`)
}

function portableIdentifier(value: unknown, path: string): string {
  if (typeof value !== 'string' || !PORTABLE_IDENTIFIER_PATTERN.test(value)) {
    operatorError('INVALID_IDENTITY', path, `${path} is not a portable identifier`)
  }
  return value
}

function boundedText(
  value: unknown,
  path: string,
  maximum: number,
  options: {
    readonly empty?: boolean
    readonly code?: 'INVALID_REQUEST' | 'INVALID_RESPONSE'
  } = {},
): string {
  if (typeof value !== 'string'
    || value.length > maximum
    || (!options.empty && value.trim().length === 0)
    || /[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f]/u.test(value)) {
    operatorError(options.code ?? 'INVALID_REQUEST', path, `${path} is not valid bounded text`)
  }
  return value
}

function nullableBoundedText(
  value: unknown,
  path: string,
  maximum: number,
  code: 'INVALID_REQUEST' | 'INVALID_RESPONSE' = 'INVALID_REQUEST',
): string | null {
  if (value === null) return null
  return boundedText(value, path, maximum, { code })
}

function nonNegativeInteger(
  value: unknown,
  path: string,
  code: 'INVALID_REQUEST' | 'INVALID_RESPONSE' = 'INVALID_RESPONSE',
): number {
  if (!Number.isSafeInteger(value) || Number(value) < 0 || Object.is(value, -0)) {
    operatorError(code, path, `${path} must be a non-negative integer`)
  }
  return Number(value)
}

function boundedPositiveInteger(value: unknown, path: string, maximum: number): number {
  if (!Number.isSafeInteger(value) || Number(value) < 1 || Number(value) > maximum) {
    operatorError('LIMIT_EXCEEDED', path, `${path} must be between 1 and ${maximum}`)
  }
  return Number(value)
}

function decimalSequence(value: unknown, path: string, allowZero = false): string {
  if (typeof value !== 'string'
    || !DECIMAL_SEQUENCE_PATTERN.test(value)
    || (!allowZero && value === '0')) {
    operatorError('INVALID_IDENTITY', path, `${path} must be a decimal sequence`)
  }
  return value
}

function booleanValue(value: unknown, path: string): boolean {
  if (typeof value !== 'boolean') operatorError('INVALID_RESPONSE', path, `${path} must be boolean`)
  return value
}

function enumValue<const Values extends readonly string[]>(
  value: unknown,
  values: Values,
  path: string,
  code: StrongFlowOperatorValidationErrorCode,
): Values[number] {
  if (typeof value !== 'string' || !values.includes(value)) {
    operatorError(code, path, `${path} is unsupported`)
  }
  return value as Values[number]
}

function parseJobId(value: unknown, path: string): JobIdentifier {
  try {
    if (typeof value !== 'string') throw new Error('job id must be a string')
    return JobId(value)
  } catch (error) {
    operatorError('INVALID_IDENTITY', path, `${path} is invalid`, { cause: error })
  }
}

function parseDefinition(
  value: unknown,
  path: string,
  code: 'INVALID_REQUEST' | 'INVALID_RESPONSE' = 'INVALID_REQUEST',
): DefinitionIdentity {
  const input = record(value, path, code)
  exactKeys(input, [
    'requirementId',
    'solutionId',
    'systemArchitectureDiagramId',
    'processFlowDiagramId',
  ], path, code)
  try {
    if (typeof input.requirementId !== 'string'
      || typeof input.solutionId !== 'string'
      || typeof input.systemArchitectureDiagramId !== 'string'
      || typeof input.processFlowDiagramId !== 'string') {
      throw new Error('definition identities must be strings')
    }
    return Object.freeze({
      requirementId: RequirementId(input.requirementId),
      solutionId: SolutionId(input.solutionId),
      systemArchitectureDiagramId: DiagramId(input.systemArchitectureDiagramId),
      processFlowDiagramId: DiagramId(input.processFlowDiagramId),
    })
  } catch (error) {
    operatorError('INVALID_IDENTITY', path, `${path} contains invalid identities`, { cause: error })
  }
}

export function StrongFlowOperatorRequestId(value: string): StrongFlowOperatorRequestId {
  return portableIdentifier(value, 'requestId') as StrongFlowOperatorRequestId
}

export interface StrongFlowOperatorParsedEventCursor {
  readonly jobId: JobIdentifier
  readonly sequence: string
  readonly eventId: string
}

export function strongFlowOperatorEventCursor(
  jobId: JobIdentifier | string,
  sequence: string,
  eventId: string,
): StrongFlowOperatorEventCursor {
  const parsedJobId = parseJobId(jobId, 'cursor.jobId')
  const parsedSequence = decimalSequence(sequence, 'cursor.sequence')
  const parsedEventId = portableIdentifier(eventId, 'cursor.eventId')
  return `sf-event-v1/${encodeURIComponent(parsedJobId)}/${parsedSequence}/${encodeURIComponent(parsedEventId)}` as StrongFlowOperatorEventCursor
}

export function parseStrongFlowOperatorEventCursor(
  value: unknown,
): StrongFlowOperatorParsedEventCursor {
  if (typeof value !== 'string') {
    operatorError('INVALID_CURSOR', 'cursor', 'event cursor must be a string')
  }
  const parts = value.split('/')
  if (parts.length !== 4 || parts[0] !== 'sf-event-v1') {
    operatorError('INVALID_CURSOR', 'cursor', 'event cursor has an unsupported shape')
  }
  try {
    const jobId = parseJobId(decodeURIComponent(parts[1]!), 'cursor.jobId')
    const sequence = decimalSequence(parts[2], 'cursor.sequence')
    const eventId = portableIdentifier(decodeURIComponent(parts[3]!), 'cursor.eventId')
    const canonical = strongFlowOperatorEventCursor(jobId, sequence, eventId)
    if (canonical !== value) operatorError('INVALID_CURSOR', 'cursor', 'event cursor is not canonical')
    return Object.freeze({ jobId, sequence, eventId })
  } catch (error) {
    if (error instanceof StrongFlowOperatorValidationError) {
      operatorError('INVALID_CURSOR', error.path, 'event cursor contains an invalid identity', {
        cause: error,
      })
    }
    operatorError('INVALID_CURSOR', 'cursor', 'event cursor could not be decoded', { cause: error })
  }
}

export type StrongFlowOperatorAuthentication =
  | {
    readonly scheme: 'local-session'
    readonly proof: string
  }
  | {
    readonly scheme: 'local-peer'
    readonly proof: string
  }

export interface StrongFlowCreateJobPayload {
  readonly repositoryPath: string
  readonly baseRevision: string | null
  readonly title: string | null
  readonly request: string
  readonly submittedFrom: HumanReviewChannel
}

export interface StrongFlowJobReferencePayload {
  readonly jobId: JobIdentifier
}

export interface StrongFlowFollowJobPayload extends StrongFlowJobReferencePayload {
  readonly afterCursor: StrongFlowOperatorEventCursor | null
  readonly limit: number
  readonly waitMillis: number
}

interface StrongFlowReviewPayloadBase extends StrongFlowJobReferencePayload {
  readonly definition: DefinitionIdentity
  readonly channel: HumanReviewChannel
  readonly authentication: StrongFlowOperatorAuthentication
  readonly comment: string | null
}

export interface StrongFlowApproveReviewPayload extends StrongFlowReviewPayloadBase {}
export interface StrongFlowRejectReviewPayload extends StrongFlowReviewPayloadBase {}

export interface StrongFlowRequestChangesPayload extends StrongFlowReviewPayloadBase {
  readonly scope: DefinitionRevisionScope
}

export interface StrongFlowCancelJobPayload extends StrongFlowJobReferencePayload {
  readonly reason: string
}

export interface StrongFlowResumeJobPayload extends StrongFlowJobReferencePayload {
  readonly interruptionSequence: string
}

export interface StrongFlowListArtifactsPayload extends StrongFlowJobReferencePayload {
  readonly afterSequence: string | null
  readonly limit: number
  readonly artifactKinds: readonly StrongFlowArtifactKind[]
}

export interface StrongFlowExportJobPayload extends StrongFlowJobReferencePayload {
  readonly format: 'manifest-json'
}

export interface StrongFlowOperatorRequestPayloadByOperation {
  readonly 'job.create': StrongFlowCreateJobPayload
  readonly 'job.status': StrongFlowJobReferencePayload
  readonly 'job.follow': StrongFlowFollowJobPayload
  readonly 'definition.requirement': StrongFlowJobReferencePayload
  readonly 'definition.solution': StrongFlowJobReferencePayload
  readonly 'definition.diagrams': StrongFlowJobReferencePayload
  readonly 'review.approve': StrongFlowApproveReviewPayload
  readonly 'review.reject': StrongFlowRejectReviewPayload
  readonly 'review.request-changes': StrongFlowRequestChangesPayload
  readonly 'job.cancel': StrongFlowCancelJobPayload
  readonly 'job.resume': StrongFlowResumeJobPayload
  readonly 'job.artifacts': StrongFlowListArtifactsPayload
  readonly 'job.export': StrongFlowExportJobPayload
}

export type StrongFlowOperatorRequestFor<Operation extends StrongFlowOperatorOperation> = {
  readonly schemaVersion: typeof STRONGFLOW_OPERATOR_SCHEMA_VERSION
  readonly requestId: StrongFlowOperatorRequestId
  readonly operation: Operation
  readonly payload: StrongFlowOperatorRequestPayloadByOperation[Operation]
}

export type StrongFlowOperatorRequest = {
  readonly [Operation in StrongFlowOperatorOperation]: StrongFlowOperatorRequestFor<Operation>
}[StrongFlowOperatorOperation]

function parseAuthentication(
  value: unknown,
  channel: HumanReviewChannel,
): StrongFlowOperatorAuthentication {
  const input = record(value, 'request.payload.authentication', 'INVALID_REQUEST')
  exactKeys(input, ['scheme', 'proof'], 'request.payload.authentication', 'INVALID_REQUEST')
  const scheme = enumValue(
    input.scheme,
    ['local-session', 'local-peer'] as const,
    'request.payload.authentication.scheme',
    'INVALID_REQUEST',
  )
  if ((channel === 'local-ui') !== (scheme === 'local-session')) {
    operatorError(
      'INVALID_REQUEST',
      'request.payload.authentication.scheme',
      'review channel and authentication scheme do not match',
    )
  }
  return Object.freeze({
    scheme,
    proof: boundedText(input.proof, 'request.payload.authentication.proof', 8_192),
  })
}

function parseJobReference(value: unknown): StrongFlowJobReferencePayload {
  const input = record(value, 'request.payload', 'INVALID_REQUEST')
  exactKeys(input, ['jobId'], 'request.payload', 'INVALID_REQUEST')
  return Object.freeze({ jobId: parseJobId(input.jobId, 'request.payload.jobId') })
}

function parseReviewPayload(
  value: unknown,
  operation: 'review.approve' | 'review.reject' | 'review.request-changes',
): StrongFlowApproveReviewPayload | StrongFlowRejectReviewPayload | StrongFlowRequestChangesPayload {
  const input = record(value, 'request.payload', 'INVALID_REQUEST')
  const changes = operation === 'review.request-changes'
  exactKeys(input, [
    'jobId',
    'definition',
    'channel',
    'authentication',
    'comment',
    ...(changes ? ['scope'] : []),
  ], 'request.payload', 'INVALID_REQUEST')
  const channel = enumValue(
    input.channel,
    ['local-ui', 'cli'] as const,
    'request.payload.channel',
    'INVALID_REQUEST',
  )
  const common = Object.freeze({
    jobId: parseJobId(input.jobId, 'request.payload.jobId'),
    definition: parseDefinition(input.definition, 'request.payload.definition'),
    channel,
    authentication: parseAuthentication(input.authentication, channel),
    comment: nullableBoundedText(input.comment, 'request.payload.comment', 4_096),
  })
  if (!changes) return common
  return Object.freeze({
    ...common,
    scope: enumValue(
      input.scope,
      ['requirements', 'solution', 'diagrams'] as const,
      'request.payload.scope',
      'INVALID_REQUEST',
    ),
  })
}

function parseRequestPayload(
  operation: StrongFlowOperatorOperation,
  value: unknown,
): StrongFlowOperatorRequestPayloadByOperation[StrongFlowOperatorOperation] {
  switch (operation) {
    case 'job.create': {
      const input = record(value, 'request.payload', 'INVALID_REQUEST')
      exactKeys(input, [
        'repositoryPath',
        'baseRevision',
        'title',
        'request',
        'submittedFrom',
      ], 'request.payload', 'INVALID_REQUEST')
      return Object.freeze({
        repositoryPath: boundedText(input.repositoryPath, 'request.payload.repositoryPath', 4_096),
        baseRevision: nullableBoundedText(
          input.baseRevision,
          'request.payload.baseRevision',
          512,
        ),
        title: nullableBoundedText(input.title, 'request.payload.title', 500),
        request: boundedText(input.request, 'request.payload.request', 1_000_000),
        submittedFrom: enumValue(
          input.submittedFrom,
          ['local-ui', 'cli'] as const,
          'request.payload.submittedFrom',
          'INVALID_REQUEST',
        ),
      })
    }
    case 'job.status':
    case 'definition.requirement':
    case 'definition.solution':
    case 'definition.diagrams': return parseJobReference(value)
    case 'job.follow': {
      const input = record(value, 'request.payload', 'INVALID_REQUEST')
      exactKeys(input, [
        'jobId',
        'afterCursor',
        'limit',
        'waitMillis',
      ], 'request.payload', 'INVALID_REQUEST')
      const jobId = parseJobId(input.jobId, 'request.payload.jobId')
      let afterCursor: StrongFlowOperatorEventCursor | null = null
      if (input.afterCursor !== null) {
        const parsed = parseStrongFlowOperatorEventCursor(input.afterCursor)
        if (parsed.jobId !== jobId) {
          operatorError(
            'INVALID_CURSOR',
            'request.payload.afterCursor',
            'event cursor belongs to another job',
          )
        }
        afterCursor = input.afterCursor as StrongFlowOperatorEventCursor
      }
      const waitMillis = nonNegativeInteger(
        input.waitMillis,
        'request.payload.waitMillis',
        'INVALID_REQUEST',
      )
      if (waitMillis > STRONGFLOW_OPERATOR_MAX_FOLLOW_WAIT_MILLIS) {
        operatorError(
          'LIMIT_EXCEEDED',
          'request.payload.waitMillis',
          'follow wait exceeds the product maximum',
        )
      }
      return Object.freeze({
        jobId,
        afterCursor,
        limit: boundedPositiveInteger(
          input.limit,
          'request.payload.limit',
          STRONGFLOW_OPERATOR_MAX_EVENT_PAGE,
        ),
        waitMillis,
      })
    }
    case 'review.approve':
    case 'review.reject':
    case 'review.request-changes': return parseReviewPayload(value, operation)
    case 'job.cancel': {
      const input = record(value, 'request.payload', 'INVALID_REQUEST')
      exactKeys(input, ['jobId', 'reason'], 'request.payload', 'INVALID_REQUEST')
      return Object.freeze({
        jobId: parseJobId(input.jobId, 'request.payload.jobId'),
        reason: boundedText(input.reason, 'request.payload.reason', 4_096),
      })
    }
    case 'job.resume': {
      const input = record(value, 'request.payload', 'INVALID_REQUEST')
      exactKeys(input, ['jobId', 'interruptionSequence'], 'request.payload', 'INVALID_REQUEST')
      return Object.freeze({
        jobId: parseJobId(input.jobId, 'request.payload.jobId'),
        interruptionSequence: decimalSequence(
          input.interruptionSequence,
          'request.payload.interruptionSequence',
        ),
      })
    }
    case 'job.artifacts': {
      const input = record(value, 'request.payload', 'INVALID_REQUEST')
      exactKeys(input, [
        'jobId',
        'afterSequence',
        'limit',
        'artifactKinds',
      ], 'request.payload', 'INVALID_REQUEST')
      if (!Array.isArray(input.artifactKinds)) {
        operatorError('INVALID_REQUEST', 'request.payload.artifactKinds', 'artifact kinds must be an array')
      }
      const artifactKinds = input.artifactKinds.map((kind, index) => enumValue(
        kind,
        STRONGFLOW_ARTIFACT_KINDS,
        `request.payload.artifactKinds[${index}]`,
        'INVALID_REQUEST',
      ))
      if (new Set(artifactKinds).size !== artifactKinds.length) {
        operatorError('INVALID_REQUEST', 'request.payload.artifactKinds', 'artifact kinds are repeated')
      }
      return Object.freeze({
        jobId: parseJobId(input.jobId, 'request.payload.jobId'),
        afterSequence: input.afterSequence === null
          ? null
          : decimalSequence(input.afterSequence, 'request.payload.afterSequence'),
        limit: boundedPositiveInteger(
          input.limit,
          'request.payload.limit',
          STRONGFLOW_OPERATOR_MAX_ARTIFACT_PAGE,
        ),
        artifactKinds: Object.freeze(artifactKinds),
      })
    }
    case 'job.export': {
      const input = record(value, 'request.payload', 'INVALID_REQUEST')
      exactKeys(input, ['jobId', 'format'], 'request.payload', 'INVALID_REQUEST')
      if (input.format !== 'manifest-json') {
        operatorError('INVALID_REQUEST', 'request.payload.format', 'export format is unsupported')
      }
      return Object.freeze({
        jobId: parseJobId(input.jobId, 'request.payload.jobId'),
        format: 'manifest-json',
      })
    }
  }
}

export function parseStrongFlowOperatorRequest(value: unknown): StrongFlowOperatorRequest {
  const input = record(value, 'request', 'INVALID_REQUEST')
  exactKeys(input, ['schemaVersion', 'requestId', 'operation', 'payload'], 'request', 'INVALID_REQUEST')
  if (input.schemaVersion !== STRONGFLOW_OPERATOR_SCHEMA_VERSION) {
    operatorError(
      'UNSUPPORTED_SCHEMA_VERSION',
      'request.schemaVersion',
      'operator request schema version is unsupported',
    )
  }
  const operation = enumValue(
    input.operation,
    STRONGFLOW_OPERATOR_OPERATIONS,
    'request.operation',
    'INVALID_REQUEST',
  )
  return Object.freeze({
    schemaVersion: STRONGFLOW_OPERATOR_SCHEMA_VERSION,
    requestId: StrongFlowOperatorRequestId(portableIdentifier(input.requestId, 'request.requestId')),
    operation,
    payload: parseRequestPayload(operation, input.payload),
  }) as StrongFlowOperatorRequest
}

export function materializeStrongFlowOperatorRequest<Operation extends StrongFlowOperatorOperation>(
  operation: Operation,
  requestId: StrongFlowOperatorRequestId | string,
  payload: StrongFlowOperatorRequestPayloadByOperation[Operation],
): StrongFlowOperatorRequestFor<Operation> {
  return parseStrongFlowOperatorRequest({
    schemaVersion: STRONGFLOW_OPERATOR_SCHEMA_VERSION,
    requestId,
    operation,
    payload,
  }) as StrongFlowOperatorRequestFor<Operation>
}

export const STRONGFLOW_OPERATOR_ERROR_CODES = Object.freeze([
  'INVALID_REQUEST',
  'UNSUPPORTED_SCHEMA_VERSION',
  'LIMIT_EXCEEDED',
  'INVALID_CURSOR',
  'JOB_NOT_FOUND',
  'ARTIFACT_NOT_FOUND',
  'JOB_CONFLICT',
  'WRONG_JOB_STATE',
  'JOB_TERMINAL',
  'STALE_DEFINITION',
  'REVIEW_ALREADY_DECIDED',
  'AUTHENTICATION_REQUIRED',
  'AUTHENTICATION_FAILED',
  'OPERATION_ABORTED',
  'STORE_FAILURE',
  'INTERNAL_ERROR',
] as const)

export type StrongFlowOperatorErrorCode = typeof STRONGFLOW_OPERATOR_ERROR_CODES[number]
export type StrongFlowOperatorErrorCategory =
  | 'usage'
  | 'not-found'
  | 'conflict'
  | 'authentication'
  | 'service'

export interface StrongFlowOperatorErrorDefinition {
  readonly category: StrongFlowOperatorErrorCategory
  readonly status: number
  readonly retryable: boolean
  readonly exitCode: 1 | 2 | 3 | 4 | 5
}

export const STRONGFLOW_OPERATOR_ERROR_DEFINITIONS: Readonly<
  Record<StrongFlowOperatorErrorCode, StrongFlowOperatorErrorDefinition>
> = Object.freeze({
  INVALID_REQUEST: Object.freeze({ category: 'usage', status: 400, retryable: false, exitCode: 2 }),
  UNSUPPORTED_SCHEMA_VERSION: Object.freeze({
    category: 'usage',
    status: 400,
    retryable: false,
    exitCode: 2,
  }),
  LIMIT_EXCEEDED: Object.freeze({ category: 'usage', status: 400, retryable: false, exitCode: 2 }),
  INVALID_CURSOR: Object.freeze({ category: 'usage', status: 400, retryable: false, exitCode: 2 }),
  JOB_NOT_FOUND: Object.freeze({ category: 'not-found', status: 404, retryable: false, exitCode: 3 }),
  ARTIFACT_NOT_FOUND: Object.freeze({
    category: 'not-found',
    status: 404,
    retryable: false,
    exitCode: 3,
  }),
  JOB_CONFLICT: Object.freeze({ category: 'conflict', status: 409, retryable: false, exitCode: 4 }),
  WRONG_JOB_STATE: Object.freeze({
    category: 'conflict',
    status: 409,
    retryable: false,
    exitCode: 4,
  }),
  JOB_TERMINAL: Object.freeze({ category: 'conflict', status: 409, retryable: false, exitCode: 4 }),
  STALE_DEFINITION: Object.freeze({
    category: 'conflict',
    status: 409,
    retryable: false,
    exitCode: 4,
  }),
  REVIEW_ALREADY_DECIDED: Object.freeze({
    category: 'conflict',
    status: 409,
    retryable: false,
    exitCode: 4,
  }),
  AUTHENTICATION_REQUIRED: Object.freeze({
    category: 'authentication',
    status: 401,
    retryable: false,
    exitCode: 5,
  }),
  AUTHENTICATION_FAILED: Object.freeze({
    category: 'authentication',
    status: 403,
    retryable: false,
    exitCode: 5,
  }),
  OPERATION_ABORTED: Object.freeze({
    category: 'service',
    status: 499,
    retryable: true,
    exitCode: 1,
  }),
  STORE_FAILURE: Object.freeze({ category: 'service', status: 503, retryable: true, exitCode: 1 }),
  INTERNAL_ERROR: Object.freeze({ category: 'service', status: 500, retryable: false, exitCode: 1 }),
})

export interface StrongFlowOperatorPublicError {
  readonly code: StrongFlowOperatorErrorCode
  readonly category: StrongFlowOperatorErrorCategory
  readonly status: number
  readonly retryable: boolean
  readonly message: string
  readonly field: string | null
  readonly currentDefinition: DefinitionIdentity | null
}

export const STRONGFLOW_OPERATOR_EXECUTION_LOCK_REASONS = Object.freeze([
  'definition-incomplete',
  'awaiting-human-review',
  'definition-revision-requested',
  'definition-approved',
  'job-active',
  'job-interrupted',
  'job-terminal',
] as const)

export type StrongFlowOperatorExecutionLockReason =
  typeof STRONGFLOW_OPERATOR_EXECUTION_LOCK_REASONS[number]

export interface StrongFlowOperatorDefinitionView {
  readonly revision: number
  readonly requirementId: string | null
  readonly solutionId: string | null
  readonly systemArchitectureDiagramId: string | null
  readonly processFlowDiagramId: string | null
}

export type StrongFlowOperatorReviewStatus =
  | 'unavailable'
  | 'pending'
  | 'approved'
  | 'changes-requested'
  | 'rejected'

export interface StrongFlowOperatorReviewView {
  readonly status: StrongFlowOperatorReviewStatus
  readonly definition: DefinitionIdentity | null
  readonly record: HumanReviewRecord | null
}

export interface StrongFlowOperatorStageRunView {
  readonly stage: StrongFlowJobStage
  readonly stageRunId: string
  readonly attemptId: string
  readonly roleId: StrongFlowRoleId | null
  readonly kernelSessionId: string | null
  readonly startedAtMillis: number
}

export interface StrongFlowOperatorInterruptionView {
  readonly sequence: string
  readonly resumeState: StrongFlowJobState
  readonly reason: string
  readonly stageRunId: string | null
}

export interface StrongFlowOperatorStopView {
  readonly kind:
    | 'task-failure'
    | 'infrastructure-failure'
    | 'human-rejection'
    | 'cancellation'
    | 'interruption'
  readonly occurredAtMillis: number
  readonly message: string
  readonly code: string | null
  readonly retryable: boolean | null
  readonly stage: StrongFlowJobStage | null
  readonly stageRunId: string | null
}

export interface StrongFlowOperatorJobView {
  readonly schemaVersion: typeof STRONGFLOW_OPERATOR_JOB_VIEW_SCHEMA_VERSION
  readonly jobId: JobIdentifier
  readonly title: string | null
  readonly state: StrongFlowJobState
  readonly sequence: string
  readonly updatedAtMillis: number
  readonly definition: StrongFlowOperatorDefinitionView
  readonly review: StrongFlowOperatorReviewView
  readonly activeStage: StrongFlowOperatorStageRunView | null
  readonly candidateId: CandidateIdentifier | null
  readonly interruption: StrongFlowOperatorInterruptionView | null
  readonly lastStop: StrongFlowOperatorStopView | null
  readonly executionLock: {
    readonly locked: boolean
    readonly reason: StrongFlowOperatorExecutionLockReason
    readonly message: string
  }
  readonly allowedOperations: readonly StrongFlowOperatorOperation[]
}

function nullableIdentifier(value: unknown, path: string): string | null {
  if (value === null) return null
  return portableIdentifier(value, path)
}

function parseDefinitionView(value: unknown): StrongFlowOperatorDefinitionView {
  const input = record(value, 'response.result.job.definition', 'INVALID_RESPONSE')
  exactKeys(input, [
    'revision',
    'requirementId',
    'solutionId',
    'systemArchitectureDiagramId',
    'processFlowDiagramId',
  ], 'response.result.job.definition', 'INVALID_RESPONSE')
  return Object.freeze({
    revision: nonNegativeInteger(input.revision, 'response.result.job.definition.revision'),
    requirementId: nullableIdentifier(
      input.requirementId,
      'response.result.job.definition.requirementId',
    ),
    solutionId: nullableIdentifier(input.solutionId, 'response.result.job.definition.solutionId'),
    systemArchitectureDiagramId: nullableIdentifier(
      input.systemArchitectureDiagramId,
      'response.result.job.definition.systemArchitectureDiagramId',
    ),
    processFlowDiagramId: nullableIdentifier(
      input.processFlowDiagramId,
      'response.result.job.definition.processFlowDiagramId',
    ),
  })
}

function parseReviewView(
  value: unknown,
  definitionView: StrongFlowOperatorDefinitionView,
): StrongFlowOperatorReviewView {
  const input = record(value, 'response.result.job.review', 'INVALID_RESPONSE')
  exactKeys(input, ['status', 'definition', 'record'], 'response.result.job.review', 'INVALID_RESPONSE')
  const status = enumValue(
    input.status,
    ['unavailable', 'pending', 'approved', 'changes-requested', 'rejected'] as const,
    'response.result.job.review.status',
    'INVALID_RESPONSE',
  )
  const definition = input.definition === null
    ? null
    : parseDefinition(
      input.definition,
      'response.result.job.review.definition',
      'INVALID_RESPONSE',
    )
  let reviewRecord: HumanReviewRecord | null = null
  if (input.record !== null) {
    try {
      reviewRecord = parseStrongFlowArtifactAs('HUMAN_REVIEW_RECORD', input.record)
    } catch (error) {
      operatorError(
        'INVALID_RESPONSE',
        'response.result.job.review.record',
        'review record is invalid',
        { cause: error },
      )
    }
  }
  if ((status === 'unavailable') !== (definition === null)) {
    operatorError(
      'RELATIONSHIP_MISMATCH',
      'response.result.job.review.definition',
      'review definition presence does not match review status',
    )
  }
  const decided = status === 'approved' || status === 'changes-requested' || status === 'rejected'
  if (decided !== (reviewRecord !== null)) {
    operatorError(
      'RELATIONSHIP_MISMATCH',
      'response.result.job.review.record',
      'review record presence does not match review status',
    )
  }
  if (definition !== null && (status === 'pending' || status === 'approved')) {
    const complete = definitionView.requirementId !== null
      && definitionView.solutionId !== null
      && definitionView.systemArchitectureDiagramId !== null
      && definitionView.processFlowDiagramId !== null
    if (!complete
      || definition.requirementId !== definitionView.requirementId
      || definition.solutionId !== definitionView.solutionId
      || definition.systemArchitectureDiagramId !== definitionView.systemArchitectureDiagramId
      || definition.processFlowDiagramId !== definitionView.processFlowDiagramId) {
      operatorError(
        'RELATIONSHIP_MISMATCH',
        'response.result.job.review.definition',
        'review definition is not the displayed definition',
      )
    }
  }
  if (reviewRecord !== null && definition !== null) {
    const decisionByStatus = {
      approved: 'approved',
      'changes-requested': 'changes-requested',
      rejected: 'rejected',
    } as const
    if (!(status in decisionByStatus)
      || reviewRecord.payload.decision !== decisionByStatus[
        status as keyof typeof decisionByStatus
      ]
      || reviewRecord.payload.definition.requirementId !== definition.requirementId
      || reviewRecord.payload.definition.solutionId !== definition.solutionId
      || reviewRecord.payload.definition.systemArchitectureDiagramId
        !== definition.systemArchitectureDiagramId
      || reviewRecord.payload.definition.processFlowDiagramId
        !== definition.processFlowDiagramId) {
      operatorError(
        'RELATIONSHIP_MISMATCH',
        'response.result.job.review.record',
        'review decision does not match review status',
      )
    }
  }
  return Object.freeze({ status, definition, record: reviewRecord })
}

function parseStageRunView(
  value: unknown,
  path = 'response.result.job.activeStage',
): StrongFlowOperatorStageRunView | null {
  if (value === null) return null
  const input = record(value, path, 'INVALID_RESPONSE')
  exactKeys(input, [
    'stage',
    'stageRunId',
    'attemptId',
    'roleId',
    'kernelSessionId',
    'startedAtMillis',
  ], path, 'INVALID_RESPONSE')
  const roleId = input.roleId === null
    ? null
    : enumValue(
      input.roleId,
      STRONGFLOW_ROLE_IDS,
      `${path}.roleId`,
      'INVALID_RESPONSE',
    )
  try {
    return Object.freeze({
      stage: enumValue(
        input.stage,
        STRONGFLOW_JOB_STAGES,
        `${path}.stage`,
        'INVALID_RESPONSE',
      ),
      stageRunId: StageRunId(portableIdentifier(
        input.stageRunId,
        `${path}.stageRunId`,
      )),
      attemptId: AttemptId(portableIdentifier(
        input.attemptId,
        `${path}.attemptId`,
      )),
      roleId,
      kernelSessionId: input.kernelSessionId === null
        ? null
        : KernelSessionId(portableIdentifier(
          input.kernelSessionId,
          `${path}.kernelSessionId`,
        )),
      startedAtMillis: nonNegativeInteger(
        input.startedAtMillis,
        `${path}.startedAtMillis`,
      ),
    })
  } catch (error) {
    operatorError(
      'INVALID_IDENTITY',
      path,
      'active stage contains an invalid identity',
      { cause: error },
    )
  }
}

function parseInterruptionView(value: unknown): StrongFlowOperatorInterruptionView | null {
  if (value === null) return null
  const input = record(value, 'response.result.job.interruption', 'INVALID_RESPONSE')
  exactKeys(input, [
    'sequence',
    'resumeState',
    'reason',
    'stageRunId',
  ], 'response.result.job.interruption', 'INVALID_RESPONSE')
  return Object.freeze({
    sequence: decimalSequence(input.sequence, 'response.result.job.interruption.sequence'),
    resumeState: enumValue(
      input.resumeState,
      STRONGFLOW_JOB_STATES,
      'response.result.job.interruption.resumeState',
      'INVALID_RESPONSE',
    ),
    reason: boundedText(input.reason, 'response.result.job.interruption.reason', 4_096, {
      code: 'INVALID_RESPONSE',
    }),
    stageRunId: nullableIdentifier(
      input.stageRunId,
      'response.result.job.interruption.stageRunId',
    ),
  })
}

function parseStopView(value: unknown): StrongFlowOperatorStopView | null {
  if (value === null) return null
  const input = record(value, 'response.result.job.lastStop', 'INVALID_RESPONSE')
  exactKeys(input, [
    'kind',
    'occurredAtMillis',
    'message',
    'code',
    'retryable',
    'stage',
    'stageRunId',
  ], 'response.result.job.lastStop', 'INVALID_RESPONSE')
  const retryable = input.retryable === null
    ? null
    : booleanValue(input.retryable, 'response.result.job.lastStop.retryable')
  return Object.freeze({
    kind: enumValue(
      input.kind,
      [
        'task-failure',
        'infrastructure-failure',
        'human-rejection',
        'cancellation',
        'interruption',
      ] as const,
      'response.result.job.lastStop.kind',
      'INVALID_RESPONSE',
    ),
    occurredAtMillis: nonNegativeInteger(
      input.occurredAtMillis,
      'response.result.job.lastStop.occurredAtMillis',
    ),
    message: boundedText(input.message, 'response.result.job.lastStop.message', 4_096, {
      code: 'INVALID_RESPONSE',
    }),
    code: input.code === null
      ? null
      : portableIdentifier(input.code, 'response.result.job.lastStop.code'),
    retryable,
    stage: input.stage === null
      ? null
      : enumValue(
        input.stage,
        STRONGFLOW_JOB_STAGES,
        'response.result.job.lastStop.stage',
        'INVALID_RESPONSE',
      ),
    stageRunId: nullableIdentifier(
      input.stageRunId,
      'response.result.job.lastStop.stageRunId',
    ),
  })
}

export function parseStrongFlowOperatorJobView(value: unknown): StrongFlowOperatorJobView {
  const input = record(value, 'response.result.job', 'INVALID_RESPONSE')
  exactKeys(input, [
    'schemaVersion',
    'jobId',
    'title',
    'state',
    'sequence',
    'updatedAtMillis',
    'definition',
    'review',
    'activeStage',
    'candidateId',
    'interruption',
    'lastStop',
    'executionLock',
    'allowedOperations',
  ], 'response.result.job', 'INVALID_RESPONSE')
  if (input.schemaVersion !== STRONGFLOW_OPERATOR_JOB_VIEW_SCHEMA_VERSION) {
    operatorError(
      'UNSUPPORTED_SCHEMA_VERSION',
      'response.result.job.schemaVersion',
      'operator job view schema version is unsupported',
    )
  }
  const definition = parseDefinitionView(input.definition)
  const review = parseReviewView(input.review, definition)
  const executionLockInput = record(
    input.executionLock,
    'response.result.job.executionLock',
    'INVALID_RESPONSE',
  )
  exactKeys(
    executionLockInput,
    ['locked', 'reason', 'message'],
    'response.result.job.executionLock',
    'INVALID_RESPONSE',
  )
  if (!Array.isArray(input.allowedOperations)) {
    operatorError(
      'INVALID_RESPONSE',
      'response.result.job.allowedOperations',
      'allowed operations must be an array',
    )
  }
  const allowedOperations = input.allowedOperations.map((operation, index) => enumValue(
    operation,
    STRONGFLOW_OPERATOR_OPERATIONS,
    `response.result.job.allowedOperations[${index}]`,
    'INVALID_RESPONSE',
  ))
  if (new Set(allowedOperations).size !== allowedOperations.length) {
    operatorError(
      'INVALID_RESPONSE',
      'response.result.job.allowedOperations',
      'allowed operations are repeated',
    )
  }
  if (allowedOperations.includes('job.create')) {
    operatorError(
      'RELATIONSHIP_MISMATCH',
      'response.result.job.allowedOperations',
      'an existing job cannot advertise the create operation',
    )
  }
  let candidateId: CandidateIdentifier | null = null
  if (input.candidateId !== null) {
    try {
      candidateId = CandidateId(portableIdentifier(
        input.candidateId,
        'response.result.job.candidateId',
      ))
    } catch (error) {
      operatorError(
        'INVALID_IDENTITY',
        'response.result.job.candidateId',
        'job candidate identity is invalid',
        { cause: error },
      )
    }
  }
  const state = enumValue(
    input.state,
    STRONGFLOW_JOB_STATES,
    'response.result.job.state',
    'INVALID_RESPONSE',
  )
  const activeStage = parseStageRunView(input.activeStage)
  const interruption = parseInterruptionView(input.interruption)
  if ((state === 'INTERRUPTED') !== (interruption !== null)) {
    operatorError(
      'RELATIONSHIP_MISMATCH',
      'response.result.job.interruption',
      'job interruption presence does not match job state',
    )
  }
  const lockReason = enumValue(
    executionLockInput.reason,
    STRONGFLOW_OPERATOR_EXECUTION_LOCK_REASONS,
    'response.result.job.executionLock.reason',
    'INVALID_RESPONSE',
  )
  const locked = booleanValue(
    executionLockInput.locked,
    'response.result.job.executionLock.locked',
  )
  if (locked === ['definition-approved', 'job-active'].includes(lockReason)) {
    operatorError(
      'RELATIONSHIP_MISMATCH',
      'response.result.job.executionLock',
      'execution lock state does not match its reason',
    )
  }
  return Object.freeze({
    schemaVersion: STRONGFLOW_OPERATOR_JOB_VIEW_SCHEMA_VERSION,
    jobId: parseJobId(input.jobId, 'response.result.job.jobId'),
    title: nullableBoundedText(input.title, 'response.result.job.title', 500, 'INVALID_RESPONSE'),
    state,
    sequence: decimalSequence(input.sequence, 'response.result.job.sequence'),
    updatedAtMillis: nonNegativeInteger(
      input.updatedAtMillis,
      'response.result.job.updatedAtMillis',
    ),
    definition,
    review,
    activeStage,
    candidateId,
    interruption,
    lastStop: parseStopView(input.lastStop),
    executionLock: Object.freeze({
      locked,
      reason: lockReason,
      message: boundedText(
        executionLockInput.message,
        'response.result.job.executionLock.message',
        1_000,
        { code: 'INVALID_RESPONSE' },
      ),
    }),
    allowedOperations: Object.freeze(allowedOperations),
  })
}

export type StrongFlowOperatorArtifactProducer =
  | {
    readonly kind: 'system'
    readonly actorId: string
  }
  | {
    readonly kind: 'human'
    readonly actorId: string
    readonly channel: HumanReviewChannel
  }
  | {
    readonly kind: 'role'
    readonly roleId: StrongFlowRoleId
    readonly stageRunId: string
    readonly attemptId: string
    readonly kernelSessionId: string
    readonly firstKernelSequence: string
    readonly lastKernelSequence: string
    readonly kernelEventCount: number
  }

export type StrongFlowOperatorCandidateReference =
  | {
    readonly kind: 'complete'
    readonly identity: StrongFlowCandidateIdentity
  }
  | {
    readonly kind: 'diff'
    readonly candidateId: CandidateIdentifier
    readonly diffId: string
  }

export interface StrongFlowOperatorArtifactLink {
  readonly schemaVersion: typeof STRONGFLOW_OPERATOR_ARTIFACT_LINK_SCHEMA_VERSION
  readonly jobId: JobIdentifier
  readonly sequence: string
  readonly recordId: string
  readonly artifactKind: StrongFlowArtifactKind
  readonly artifactId: string
  readonly blobId: string
  readonly byteLength: number
  readonly mediaType: string
  readonly createdAtMillis: number
  readonly producer: StrongFlowOperatorArtifactProducer
  readonly candidate: StrongFlowOperatorCandidateReference | null
}

function parseArtifactProducer(value: unknown): StrongFlowOperatorArtifactProducer {
  const input = record(value, 'response.result.link.producer', 'INVALID_RESPONSE')
  if (input.kind === 'system') {
    exactKeys(input, ['kind', 'actorId'], 'response.result.link.producer', 'INVALID_RESPONSE')
    return Object.freeze({
      kind: 'system',
      actorId: portableIdentifier(input.actorId, 'response.result.link.producer.actorId'),
    })
  }
  if (input.kind === 'human') {
    exactKeys(
      input,
      ['kind', 'actorId', 'channel'],
      'response.result.link.producer',
      'INVALID_RESPONSE',
    )
    return Object.freeze({
      kind: 'human',
      actorId: portableIdentifier(input.actorId, 'response.result.link.producer.actorId'),
      channel: enumValue(
        input.channel,
        ['local-ui', 'cli'] as const,
        'response.result.link.producer.channel',
        'INVALID_RESPONSE',
      ),
    })
  }
  if (input.kind !== 'role') {
    operatorError(
      'INVALID_RESPONSE',
      'response.result.link.producer.kind',
      'artifact producer kind is unsupported',
    )
  }
  exactKeys(input, [
    'kind',
    'roleId',
    'stageRunId',
    'attemptId',
    'kernelSessionId',
    'firstKernelSequence',
    'lastKernelSequence',
    'kernelEventCount',
  ], 'response.result.link.producer', 'INVALID_RESPONSE')
  try {
    const firstKernelSequence = decimalSequence(
      input.firstKernelSequence,
      'response.result.link.producer.firstKernelSequence',
    )
    const lastKernelSequence = decimalSequence(
      input.lastKernelSequence,
      'response.result.link.producer.lastKernelSequence',
    )
    const kernelEventCount = boundedPositiveInteger(
      input.kernelEventCount,
      'response.result.link.producer.kernelEventCount',
      Number.MAX_SAFE_INTEGER,
    )
    if (BigInt(lastKernelSequence) - BigInt(firstKernelSequence) + 1n !== BigInt(kernelEventCount)) {
      operatorError(
        'RELATIONSHIP_MISMATCH',
        'response.result.link.producer.kernelEventCount',
        'artifact producer kernel event range is not consecutive',
      )
    }
    return Object.freeze({
      kind: 'role',
      roleId: enumValue(
        input.roleId,
        STRONGFLOW_ROLE_IDS,
        'response.result.link.producer.roleId',
        'INVALID_RESPONSE',
      ),
      stageRunId: StageRunId(portableIdentifier(
        input.stageRunId,
        'response.result.link.producer.stageRunId',
      )),
      attemptId: AttemptId(portableIdentifier(
        input.attemptId,
        'response.result.link.producer.attemptId',
      )),
      kernelSessionId: KernelSessionId(portableIdentifier(
        input.kernelSessionId,
        'response.result.link.producer.kernelSessionId',
      )),
      firstKernelSequence,
      lastKernelSequence,
      kernelEventCount,
    })
  } catch (error) {
    if (error instanceof StrongFlowOperatorValidationError) throw error
    operatorError(
      'INVALID_IDENTITY',
      'response.result.link.producer',
      'artifact producer contains an invalid identity',
      { cause: error },
    )
  }
}

export function parseStrongFlowOperatorArtifactLink(
  value: unknown,
): StrongFlowOperatorArtifactLink {
  const input = record(value, 'response.result.link', 'INVALID_RESPONSE')
  exactKeys(input, [
    'schemaVersion',
    'jobId',
    'sequence',
    'recordId',
    'artifactKind',
    'artifactId',
    'blobId',
    'byteLength',
    'mediaType',
    'createdAtMillis',
    'producer',
    'candidate',
  ], 'response.result.link', 'INVALID_RESPONSE')
  if (input.schemaVersion !== STRONGFLOW_OPERATOR_ARTIFACT_LINK_SCHEMA_VERSION) {
    operatorError(
      'UNSUPPORTED_SCHEMA_VERSION',
      'response.result.link.schemaVersion',
      'artifact link schema version is unsupported',
    )
  }
  if (typeof input.blobId !== 'string' || !BLOB_ID_PATTERN.test(input.blobId)) {
    operatorError('INVALID_IDENTITY', 'response.result.link.blobId', 'artifact blob id is invalid')
  }
  if (typeof input.mediaType !== 'string' || !MEDIA_TYPE_PATTERN.test(input.mediaType)) {
    operatorError('INVALID_RESPONSE', 'response.result.link.mediaType', 'artifact media type is invalid')
  }
  const artifactKind = enumValue(
    input.artifactKind,
    STRONGFLOW_ARTIFACT_KINDS,
    'response.result.link.artifactKind',
    'INVALID_RESPONSE',
  )
  const producer = parseArtifactProducer(input.producer)
  let candidate: StrongFlowOperatorCandidateReference | null = null
  if (input.candidate !== null) {
    const candidateInput = record(
      input.candidate,
      'response.result.link.candidate',
      'INVALID_RESPONSE',
    )
    if (candidateInput.kind === 'complete') {
      exactKeys(
        candidateInput,
        ['kind', 'identity'],
        'response.result.link.candidate',
        'INVALID_RESPONSE',
      )
      try {
        candidate = Object.freeze({
          kind: 'complete',
          identity: parseStrongFlowCandidateIdentity(
            candidateInput.identity,
            'response.result.link.candidate.identity',
          ),
        })
      } catch (error) {
        operatorError(
          'INVALID_RESPONSE',
          'response.result.link.candidate.identity',
          'artifact candidate identity is invalid',
          { cause: error },
        )
      }
    } else if (candidateInput.kind === 'diff') {
      exactKeys(
        candidateInput,
        ['kind', 'candidateId', 'diffId'],
        'response.result.link.candidate',
        'INVALID_RESPONSE',
      )
      if (typeof candidateInput.diffId !== 'string'
        || !SHA256_PATTERN.test(candidateInput.diffId)) {
        operatorError(
          'INVALID_IDENTITY',
          'response.result.link.candidate.diffId',
          'artifact candidate diff identity is invalid',
        )
      }
      try {
        candidate = Object.freeze({
          kind: 'diff',
          candidateId: CandidateId(portableIdentifier(
            candidateInput.candidateId,
            'response.result.link.candidate.candidateId',
          )),
          diffId: candidateInput.diffId,
        })
      } catch (error) {
        if (error instanceof StrongFlowOperatorValidationError) throw error
        operatorError(
          'INVALID_IDENTITY',
          'response.result.link.candidate.candidateId',
          'artifact annotation candidate identity is invalid',
          { cause: error },
        )
      }
    } else {
      operatorError(
        'INVALID_RESPONSE',
        'response.result.link.candidate.kind',
        'artifact candidate reference kind is unsupported',
      )
    }
  }
  const requiresCompleteCandidate = [
    'PATCH_MANIFEST',
    'REVIEW_REPORT',
    'VERIFICATION_REPORT',
    'REMEDIATION_REQUEST',
    'REMEDIATION_REPORT',
    'DELIVERY_RECEIPT',
  ].includes(artifactKind)
  const requiresDiffCandidate = artifactKind === 'EXECUTION_CHANGE_ANNOTATION'
  if ((requiresCompleteCandidate && candidate?.kind !== 'complete')
    || (requiresDiffCandidate && candidate?.kind !== 'diff')
    || (!requiresCompleteCandidate && !requiresDiffCandidate && candidate !== null)) {
    operatorError(
      'RELATIONSHIP_MISMATCH',
      'response.result.link.candidate',
      'artifact candidate reference does not match the artifact kind',
    )
  }
  const roleProducers: Partial<Record<StrongFlowArtifactKind, readonly StrongFlowRoleId[]>> = {
    REQUIREMENT_SPEC: ['requirements'],
    SOLUTION_DESIGN: ['solution'],
    SYSTEM_ARCHITECTURE_DIAGRAM: ['solution'],
    PROCESS_FLOW_DIAGRAM: ['solution'],
    EXECUTION_PLAN: ['planner'],
    PATCH_MANIFEST: ['executor', 'remediator'],
    REVIEW_REPORT: ['reviewer'],
    VERIFICATION_REPORT: ['verifier', 'adversarial-verifier'],
    REMEDIATION_REPORT: ['remediator'],
  }
  const allowedRoles = roleProducers[artifactKind]
  const humanProduced = artifactKind === 'HUMAN_REVIEW_RECORD'
    || artifactKind === 'EXECUTION_CHANGE_ANNOTATION'
  const systemProduced = artifactKind === 'REMEDIATION_REQUEST'
    || artifactKind === 'DELIVERY_RECEIPT'
  if ((allowedRoles !== undefined
      && (producer.kind !== 'role' || !allowedRoles.includes(producer.roleId)))
    || (humanProduced && producer.kind !== 'human')
    || (systemProduced && producer.kind !== 'system')
    || (artifactKind === 'USER_REQUEST' && producer.kind === 'role')) {
    operatorError(
      'RELATIONSHIP_MISMATCH',
      'response.result.link.producer',
      'artifact producer does not match the artifact kind',
    )
  }
  return Object.freeze({
    schemaVersion: STRONGFLOW_OPERATOR_ARTIFACT_LINK_SCHEMA_VERSION,
    jobId: parseJobId(input.jobId, 'response.result.link.jobId'),
    sequence: decimalSequence(input.sequence, 'response.result.link.sequence'),
    recordId: portableIdentifier(input.recordId, 'response.result.link.recordId'),
    artifactKind,
    artifactId: portableIdentifier(input.artifactId, 'response.result.link.artifactId'),
    blobId: input.blobId,
    byteLength: boundedPositiveInteger(
      input.byteLength,
      'response.result.link.byteLength',
      Number.MAX_SAFE_INTEGER,
    ),
    mediaType: input.mediaType,
    createdAtMillis: nonNegativeInteger(
      input.createdAtMillis,
      'response.result.link.createdAtMillis',
    ),
    producer,
    candidate,
  })
}

export const STRONGFLOW_OPERATOR_EVENT_KINDS = Object.freeze([
  'job.created',
  'stage.started',
  'stage.succeeded',
  'stage.failed',
  'human-review.approved',
  'human-review.changes-requested',
  'human-review.rejected',
  'job.interrupted',
  'job.resumed',
  'job.cancelled',
  'completion-gate.passed',
  'completion-gate.failed',
  'job.delivered',
  'artifact.published',
  'evidence.published',
  'diff.updated',
  'notice',
] as const)

export type StrongFlowOperatorEventKind = typeof STRONGFLOW_OPERATOR_EVENT_KINDS[number]

export type StrongFlowOperatorEventSource =
  | {
    readonly kind: 'system'
    readonly actorId: string
  }
  | {
    readonly kind: 'human'
    readonly actorId: string
    readonly channel: HumanReviewChannel
  }
  | {
    readonly kind: 'role'
    readonly actorId: string
    readonly roleId: StrongFlowRoleId
    readonly kernelSessionId: string | null
  }

export interface StrongFlowOperatorChangedNode {
  readonly diagramId: string
  readonly nodeId: string
}

export interface StrongFlowOperatorChangeView {
  readonly state: 'executing' | 'execution-finished'
  readonly detailAccess: 'denied' | 'available'
  readonly changedPaths: readonly string[]
  readonly affectedNodes: readonly StrongFlowOperatorChangedNode[]
}

export interface StrongFlowOperatorEventView {
  readonly schemaVersion: typeof STRONGFLOW_OPERATOR_EVENT_SCHEMA_VERSION
  readonly eventId: string
  readonly cursor: StrongFlowOperatorEventCursor
  readonly jobId: JobIdentifier
  readonly sequence: string
  readonly occurredAtMillis: number
  readonly kind: StrongFlowOperatorEventKind
  readonly state: StrongFlowJobState
  readonly source: StrongFlowOperatorEventSource
  readonly stage: StrongFlowOperatorStageRunView | null
  readonly candidateId: CandidateIdentifier | null
  readonly definition: DefinitionIdentity | null
  readonly artifactLinks: readonly StrongFlowOperatorArtifactLink[]
  readonly change: StrongFlowOperatorChangeView | null
  readonly message: string
}

export interface StrongFlowOperatorEventPage {
  readonly schemaVersion: typeof STRONGFLOW_OPERATOR_EVENT_SCHEMA_VERSION
  readonly jobId: JobIdentifier
  readonly afterCursor: StrongFlowOperatorEventCursor | null
  readonly events: readonly StrongFlowOperatorEventView[]
  readonly nextCursor: StrongFlowOperatorEventCursor | null
  readonly caughtUp: boolean
}

function parseEventSource(value: unknown): StrongFlowOperatorEventSource {
  const input = record(value, 'response.result.event.source', 'INVALID_RESPONSE')
  if (input.kind === 'system') {
    exactKeys(input, ['kind', 'actorId'], 'response.result.event.source', 'INVALID_RESPONSE')
    return Object.freeze({
      kind: 'system',
      actorId: portableIdentifier(input.actorId, 'response.result.event.source.actorId'),
    })
  }
  if (input.kind === 'human') {
    exactKeys(
      input,
      ['kind', 'actorId', 'channel'],
      'response.result.event.source',
      'INVALID_RESPONSE',
    )
    return Object.freeze({
      kind: 'human',
      actorId: portableIdentifier(input.actorId, 'response.result.event.source.actorId'),
      channel: enumValue(
        input.channel,
        ['local-ui', 'cli'] as const,
        'response.result.event.source.channel',
        'INVALID_RESPONSE',
      ),
    })
  }
  if (input.kind !== 'role') {
    operatorError(
      'INVALID_RESPONSE',
      'response.result.event.source.kind',
      'event source kind is unsupported',
    )
  }
  exactKeys(
    input,
    ['kind', 'actorId', 'roleId', 'kernelSessionId'],
    'response.result.event.source',
    'INVALID_RESPONSE',
  )
  return Object.freeze({
    kind: 'role',
    actorId: portableIdentifier(input.actorId, 'response.result.event.source.actorId'),
    roleId: enumValue(
      input.roleId,
      STRONGFLOW_ROLE_IDS,
      'response.result.event.source.roleId',
      'INVALID_RESPONSE',
    ),
    kernelSessionId: nullableIdentifier(
      input.kernelSessionId,
      'response.result.event.source.kernelSessionId',
    ),
  })
}

function portableRelativePath(value: unknown, path: string): string {
  const result = boundedText(value, path, 4_096, { code: 'INVALID_RESPONSE' })
  if (result.startsWith('/')
    || result.includes('\\')
    || /[\r\n\t]/u.test(result)
    || result.split('/').some(segment => segment === '' || segment === '.' || segment === '..')) {
    operatorError('INVALID_RESPONSE', path, `${path} is not a portable relative path`)
  }
  return result
}

function parseChangeView(value: unknown): StrongFlowOperatorChangeView | null {
  if (value === null) return null
  const input = record(value, 'response.result.event.change', 'INVALID_RESPONSE')
  exactKeys(
    input,
    ['state', 'detailAccess', 'changedPaths', 'affectedNodes'],
    'response.result.event.change',
    'INVALID_RESPONSE',
  )
  const state = enumValue(
    input.state,
    ['executing', 'execution-finished'] as const,
    'response.result.event.change.state',
    'INVALID_RESPONSE',
  )
  const detailAccess = enumValue(
    input.detailAccess,
    ['denied', 'available'] as const,
    'response.result.event.change.detailAccess',
    'INVALID_RESPONSE',
  )
  if ((state === 'executing') !== (detailAccess === 'denied')) {
    operatorError(
      'RELATIONSHIP_MISMATCH',
      'response.result.event.change.detailAccess',
      'change detail access does not match execution state',
    )
  }
  if (!Array.isArray(input.changedPaths)) {
    operatorError(
      'INVALID_RESPONSE',
      'response.result.event.change.changedPaths',
      'changed paths must be an array',
    )
  }
  const changedPaths = input.changedPaths.map((entry, index) => portableRelativePath(
    entry,
    `response.result.event.change.changedPaths[${index}]`,
  ))
  if (new Set(changedPaths).size !== changedPaths.length) {
    operatorError(
      'INVALID_RESPONSE',
      'response.result.event.change.changedPaths',
      'changed paths are repeated',
    )
  }
  if (!Array.isArray(input.affectedNodes)) {
    operatorError(
      'INVALID_RESPONSE',
      'response.result.event.change.affectedNodes',
      'affected nodes must be an array',
    )
  }
  const nodeKeys = new Set<string>()
  const affectedNodes = input.affectedNodes.map((entry, index) => {
    const node = record(
      entry,
      `response.result.event.change.affectedNodes[${index}]`,
      'INVALID_RESPONSE',
    )
    exactKeys(
      node,
      ['diagramId', 'nodeId'],
      `response.result.event.change.affectedNodes[${index}]`,
      'INVALID_RESPONSE',
    )
    const result = Object.freeze({
      diagramId: portableIdentifier(
        node.diagramId,
        `response.result.event.change.affectedNodes[${index}].diagramId`,
      ),
      nodeId: portableIdentifier(
        node.nodeId,
        `response.result.event.change.affectedNodes[${index}].nodeId`,
      ),
    })
    const key = `${result.diagramId}\u0000${result.nodeId}`
    if (nodeKeys.has(key)) {
      operatorError(
        'INVALID_RESPONSE',
        `response.result.event.change.affectedNodes[${index}]`,
        'affected diagram node is repeated',
      )
    }
    nodeKeys.add(key)
    return result
  })
  return Object.freeze({
    state,
    detailAccess,
    changedPaths: Object.freeze(changedPaths),
    affectedNodes: Object.freeze(affectedNodes),
  })
}

export function parseStrongFlowOperatorEventView(value: unknown): StrongFlowOperatorEventView {
  const input = record(value, 'response.result.event', 'INVALID_RESPONSE')
  exactKeys(input, [
    'schemaVersion',
    'eventId',
    'cursor',
    'jobId',
    'sequence',
    'occurredAtMillis',
    'kind',
    'state',
    'source',
    'stage',
    'candidateId',
    'definition',
    'artifactLinks',
    'change',
    'message',
  ], 'response.result.event', 'INVALID_RESPONSE')
  if (input.schemaVersion !== STRONGFLOW_OPERATOR_EVENT_SCHEMA_VERSION) {
    operatorError(
      'UNSUPPORTED_SCHEMA_VERSION',
      'response.result.event.schemaVersion',
      'operator event schema version is unsupported',
    )
  }
  const jobId = parseJobId(input.jobId, 'response.result.event.jobId')
  const sequence = decimalSequence(input.sequence, 'response.result.event.sequence')
  const eventId = portableIdentifier(input.eventId, 'response.result.event.eventId')
  const cursor = parseStrongFlowOperatorEventCursor(input.cursor)
  if (cursor.jobId !== jobId || cursor.sequence !== sequence || cursor.eventId !== eventId) {
    operatorError(
      'RELATIONSHIP_MISMATCH',
      'response.result.event.cursor',
      'event cursor does not match the event identity',
    )
  }
  if (!Array.isArray(input.artifactLinks)) {
    operatorError(
      'INVALID_RESPONSE',
      'response.result.event.artifactLinks',
      'event artifact links must be an array',
    )
  }
  const artifactLinks = input.artifactLinks.map(parseStrongFlowOperatorArtifactLink)
  if (artifactLinks.some(link => link.jobId !== jobId)) {
    operatorError(
      'RELATIONSHIP_MISMATCH',
      'response.result.event.artifactLinks',
      'event artifact link belongs to another job',
    )
  }
  let candidateId: CandidateIdentifier | null = null
  if (input.candidateId !== null) {
    try {
      candidateId = CandidateId(portableIdentifier(
        input.candidateId,
        'response.result.event.candidateId',
      ))
    } catch (error) {
      operatorError(
        'INVALID_IDENTITY',
        'response.result.event.candidateId',
        'event candidate identity is invalid',
        { cause: error },
      )
    }
  }
  const change = parseChangeView(input.change)
  if (change !== null
    && ((change.state === 'executing' && candidateId !== null)
      || (change.state === 'execution-finished' && candidateId === null))) {
    operatorError(
      'RELATIONSHIP_MISMATCH',
      'response.result.event.candidateId',
      'change candidate presence does not match execution state',
    )
  }
  const definition = input.definition === null
    ? null
    : parseDefinition(
      input.definition,
      'response.result.event.definition',
      'INVALID_RESPONSE',
    )
  if (change !== null
    && (definition === null || change.affectedNodes.some(node => (
      node.diagramId !== definition.systemArchitectureDiagramId
      && node.diagramId !== definition.processFlowDiagramId
    )))) {
    operatorError(
      'RELATIONSHIP_MISMATCH',
      'response.result.event.change.affectedNodes',
      'change nodes do not belong to the event definition diagrams',
    )
  }
  return Object.freeze({
    schemaVersion: STRONGFLOW_OPERATOR_EVENT_SCHEMA_VERSION,
    eventId,
    cursor: input.cursor as StrongFlowOperatorEventCursor,
    jobId,
    sequence,
    occurredAtMillis: nonNegativeInteger(
      input.occurredAtMillis,
      'response.result.event.occurredAtMillis',
    ),
    kind: enumValue(
      input.kind,
      STRONGFLOW_OPERATOR_EVENT_KINDS,
      'response.result.event.kind',
      'INVALID_RESPONSE',
    ),
    state: enumValue(
      input.state,
      STRONGFLOW_JOB_STATES,
      'response.result.event.state',
      'INVALID_RESPONSE',
    ),
    source: parseEventSource(input.source),
    stage: parseStageRunView(input.stage, 'response.result.event.stage'),
    candidateId,
    definition,
    artifactLinks: Object.freeze(artifactLinks),
    change,
    message: boundedText(input.message, 'response.result.event.message', 4_096, {
      empty: true,
      code: 'INVALID_RESPONSE',
    }),
  })
}

export function parseStrongFlowOperatorEventPage(value: unknown): StrongFlowOperatorEventPage {
  const input = record(value, 'response.result.page', 'INVALID_RESPONSE')
  exactKeys(input, [
    'schemaVersion',
    'jobId',
    'afterCursor',
    'events',
    'nextCursor',
    'caughtUp',
  ], 'response.result.page', 'INVALID_RESPONSE')
  if (input.schemaVersion !== STRONGFLOW_OPERATOR_EVENT_SCHEMA_VERSION) {
    operatorError(
      'UNSUPPORTED_SCHEMA_VERSION',
      'response.result.page.schemaVersion',
      'operator event page schema version is unsupported',
    )
  }
  const jobId = parseJobId(input.jobId, 'response.result.page.jobId')
  const parsePageCursor = (cursorValue: unknown, path: string): StrongFlowOperatorEventCursor | null => {
    if (cursorValue === null) return null
    const parsed = parseStrongFlowOperatorEventCursor(cursorValue)
    if (parsed.jobId !== jobId) {
      operatorError('RELATIONSHIP_MISMATCH', path, 'event page cursor belongs to another job')
    }
    return cursorValue as StrongFlowOperatorEventCursor
  }
  const afterCursor = parsePageCursor(input.afterCursor, 'response.result.page.afterCursor')
  if (!Array.isArray(input.events) || input.events.length > STRONGFLOW_OPERATOR_MAX_EVENT_PAGE) {
    operatorError(
      'LIMIT_EXCEEDED',
      'response.result.page.events',
      'event page exceeds its product maximum',
    )
  }
  const events = input.events.map(parseStrongFlowOperatorEventView)
  if (events.some(event => event.jobId !== jobId)) {
    operatorError(
      'RELATIONSHIP_MISMATCH',
      'response.result.page.events',
      'event page contains another job',
    )
  }
  if (new Set(events.map(event => event.eventId)).size !== events.length) {
    operatorError(
      'RELATIONSHIP_MISMATCH',
      'response.result.page.events',
      'event page repeats an event identity',
    )
  }
  for (let index = 1; index < events.length; index += 1) {
    if (BigInt(events[index - 1]!.sequence) + 1n !== BigInt(events[index]!.sequence)) {
      operatorError(
        'RELATIONSHIP_MISMATCH',
        `response.result.page.events[${index}].sequence`,
        'event page sequence is not consecutive',
      )
    }
  }
  if (afterCursor !== null && events.length > 0) {
    const after = parseStrongFlowOperatorEventCursor(afterCursor)
    if (BigInt(events[0]!.sequence) !== BigInt(after.sequence) + 1n) {
      operatorError(
        'RELATIONSHIP_MISMATCH',
        'response.result.page.events[0].sequence',
        'event page did not continue directly after its cursor',
      )
    }
  }
  const nextCursor = parsePageCursor(input.nextCursor, 'response.result.page.nextCursor')
  const expectedNext = events.at(-1)?.cursor ?? afterCursor
  if (nextCursor !== expectedNext) {
    operatorError(
      'RELATIONSHIP_MISMATCH',
      'response.result.page.nextCursor',
      'event page next cursor does not identify its last event',
    )
  }
  return Object.freeze({
    schemaVersion: STRONGFLOW_OPERATOR_EVENT_SCHEMA_VERSION,
    jobId,
    afterCursor,
    events: Object.freeze(events),
    nextCursor,
    caughtUp: booleanValue(input.caughtUp, 'response.result.page.caughtUp'),
  })
}

export interface StrongFlowOperatorArtifactReadResult<Artifact extends StrongFlowArtifact> {
  readonly job: StrongFlowOperatorJobView
  readonly link: StrongFlowOperatorArtifactLink
  readonly artifact: Artifact
}

export interface StrongFlowOperatorDiagramsResult {
  readonly job: StrongFlowOperatorJobView
  readonly definition: DefinitionIdentity
  readonly systemArchitecture: {
    readonly link: StrongFlowOperatorArtifactLink
    readonly artifact: SystemArchitectureDiagram
  }
  readonly processFlow: {
    readonly link: StrongFlowOperatorArtifactLink
    readonly artifact: ProcessFlowDiagram
  }
}

export interface StrongFlowOperatorMutationReceipt {
  readonly job: StrongFlowOperatorJobView
  readonly event: StrongFlowOperatorEventView
  readonly review: HumanReviewRecord | null
}

export interface StrongFlowOperatorArtifactPage {
  readonly schemaVersion: typeof STRONGFLOW_OPERATOR_ARTIFACT_LINK_SCHEMA_VERSION
  readonly jobId: JobIdentifier
  readonly afterSequence: string | null
  readonly artifacts: readonly StrongFlowOperatorArtifactLink[]
  readonly nextAfterSequence: string | null
}

export interface StrongFlowOperatorExportManifest {
  readonly schemaVersion: typeof STRONGFLOW_OPERATOR_EXPORT_SCHEMA_VERSION
  readonly format: 'manifest-json'
  readonly exportedAtMillis: number
  readonly job: StrongFlowOperatorJobView
  readonly events: readonly StrongFlowOperatorEventView[]
  readonly artifacts: readonly StrongFlowOperatorArtifactLink[]
}

export interface StrongFlowOperatorResponseResultByOperation {
  readonly 'job.create': { readonly job: StrongFlowOperatorJobView }
  readonly 'job.status': { readonly job: StrongFlowOperatorJobView }
  readonly 'job.follow': StrongFlowOperatorEventPage
  readonly 'definition.requirement': StrongFlowOperatorArtifactReadResult<RequirementSpec>
  readonly 'definition.solution': StrongFlowOperatorArtifactReadResult<SolutionDesign>
  readonly 'definition.diagrams': StrongFlowOperatorDiagramsResult
  readonly 'review.approve': StrongFlowOperatorMutationReceipt
  readonly 'review.reject': StrongFlowOperatorMutationReceipt
  readonly 'review.request-changes': StrongFlowOperatorMutationReceipt
  readonly 'job.cancel': StrongFlowOperatorMutationReceipt
  readonly 'job.resume': StrongFlowOperatorMutationReceipt
  readonly 'job.artifacts': StrongFlowOperatorArtifactPage
  readonly 'job.export': StrongFlowOperatorExportManifest
}

export type StrongFlowOperatorSuccessResponseFor<
  Operation extends StrongFlowOperatorOperation,
> = {
  readonly schemaVersion: typeof STRONGFLOW_OPERATOR_SCHEMA_VERSION
  readonly requestId: StrongFlowOperatorRequestId
  readonly operation: Operation
  readonly ok: true
  readonly result: StrongFlowOperatorResponseResultByOperation[Operation]
}

export type StrongFlowOperatorSuccessResponse = {
  readonly [Operation in StrongFlowOperatorOperation]: StrongFlowOperatorSuccessResponseFor<Operation>
}[StrongFlowOperatorOperation]

export interface StrongFlowOperatorFailureResponse {
  readonly schemaVersion: typeof STRONGFLOW_OPERATOR_SCHEMA_VERSION
  readonly requestId: StrongFlowOperatorRequestId | null
  readonly operation: StrongFlowOperatorOperation | null
  readonly ok: false
  readonly error: StrongFlowOperatorPublicError
}

export type StrongFlowOperatorResponse =
  | StrongFlowOperatorSuccessResponse
  | StrongFlowOperatorFailureResponse

export type StrongFlowOperatorResponseFor<Operation extends StrongFlowOperatorOperation> =
  | StrongFlowOperatorSuccessResponseFor<Operation>
  | StrongFlowOperatorFailureResponse

function assertArtifactLink(
  link: StrongFlowOperatorArtifactLink,
  artifact: StrongFlowArtifact,
  job: StrongFlowOperatorJobView,
): void {
  if (link.jobId !== job.jobId
    || artifact.jobId !== job.jobId
    || link.artifactKind !== artifact.artifactKind
    || link.artifactId !== artifact.artifactId
    || link.createdAtMillis !== artifact.createdAtMillis) {
    operatorError(
      'RELATIONSHIP_MISMATCH',
      'response.result.link',
      'artifact link does not identify the returned artifact and job',
    )
  }
}

function parseArtifactReadResult(
  value: unknown,
  kind: 'REQUIREMENT_SPEC' | 'SOLUTION_DESIGN',
): StrongFlowOperatorArtifactReadResult<RequirementSpec | SolutionDesign> {
  const input = record(value, 'response.result', 'INVALID_RESPONSE')
  exactKeys(input, ['job', 'link', 'artifact'], 'response.result', 'INVALID_RESPONSE')
  const job = parseStrongFlowOperatorJobView(input.job)
  const link = parseStrongFlowOperatorArtifactLink(input.link)
  let artifact: RequirementSpec | SolutionDesign
  try {
    artifact = kind === 'REQUIREMENT_SPEC'
      ? parseStrongFlowArtifactAs('REQUIREMENT_SPEC', input.artifact)
      : parseStrongFlowArtifactAs('SOLUTION_DESIGN', input.artifact)
  } catch (error) {
    operatorError('INVALID_RESPONSE', 'response.result.artifact', 'definition artifact is invalid', {
      cause: error,
    })
  }
  assertArtifactLink(link, artifact, job)
  const expectedId = kind === 'REQUIREMENT_SPEC'
    ? job.definition.requirementId
    : job.definition.solutionId
  if (artifact.artifactId !== expectedId) {
    operatorError(
      'RELATIONSHIP_MISMATCH',
      'response.result.artifact.artifactId',
      'definition read does not match the current job definition',
    )
  }
  return Object.freeze({ job, link, artifact })
}

function parseDiagramEntry(
  value: unknown,
  kind: 'SYSTEM_ARCHITECTURE_DIAGRAM',
): {
  readonly link: StrongFlowOperatorArtifactLink
  readonly artifact: SystemArchitectureDiagram
}
function parseDiagramEntry(
  value: unknown,
  kind: 'PROCESS_FLOW_DIAGRAM',
): {
  readonly link: StrongFlowOperatorArtifactLink
  readonly artifact: ProcessFlowDiagram
}
function parseDiagramEntry(
  value: unknown,
  kind: 'SYSTEM_ARCHITECTURE_DIAGRAM' | 'PROCESS_FLOW_DIAGRAM',
): {
  readonly link: StrongFlowOperatorArtifactLink
  readonly artifact: SystemArchitectureDiagram | ProcessFlowDiagram
} {
  const path = kind === 'SYSTEM_ARCHITECTURE_DIAGRAM'
    ? 'response.result.systemArchitecture'
    : 'response.result.processFlow'
  const input = record(value, path, 'INVALID_RESPONSE')
  exactKeys(input, ['link', 'artifact'], path, 'INVALID_RESPONSE')
  const link = parseStrongFlowOperatorArtifactLink(input.link)
  let artifact: SystemArchitectureDiagram | ProcessFlowDiagram
  try {
    artifact = kind === 'SYSTEM_ARCHITECTURE_DIAGRAM'
      ? parseStrongFlowArtifactAs('SYSTEM_ARCHITECTURE_DIAGRAM', input.artifact)
      : parseStrongFlowArtifactAs('PROCESS_FLOW_DIAGRAM', input.artifact)
  } catch (error) {
    operatorError('INVALID_RESPONSE', `${path}.artifact`, 'diagram artifact is invalid', {
      cause: error,
    })
  }
  return Object.freeze({ link, artifact })
}

function parseDiagramsResult(value: unknown): StrongFlowOperatorDiagramsResult {
  const input = record(value, 'response.result', 'INVALID_RESPONSE')
  exactKeys(
    input,
    ['job', 'definition', 'systemArchitecture', 'processFlow'],
    'response.result',
    'INVALID_RESPONSE',
  )
  const job = parseStrongFlowOperatorJobView(input.job)
  const definition = parseDefinition(
    input.definition,
    'response.result.definition',
    'INVALID_RESPONSE',
  )
  const systemArchitecture = parseDiagramEntry(
    input.systemArchitecture,
    'SYSTEM_ARCHITECTURE_DIAGRAM',
  )
  const processFlow = parseDiagramEntry(input.processFlow, 'PROCESS_FLOW_DIAGRAM')
  assertArtifactLink(systemArchitecture.link, systemArchitecture.artifact, job)
  assertArtifactLink(processFlow.link, processFlow.artifact, job)
  if (definition.requirementId !== job.definition.requirementId
    || definition.solutionId !== job.definition.solutionId
    || definition.systemArchitectureDiagramId !== job.definition.systemArchitectureDiagramId
    || definition.processFlowDiagramId !== job.definition.processFlowDiagramId
    || systemArchitecture.artifact.artifactId !== definition.systemArchitectureDiagramId
    || processFlow.artifact.artifactId !== definition.processFlowDiagramId
    || systemArchitecture.artifact.payload.requirementId !== definition.requirementId
    || processFlow.artifact.payload.requirementId !== definition.requirementId
    || systemArchitecture.artifact.payload.solutionId !== definition.solutionId
    || processFlow.artifact.payload.solutionId !== definition.solutionId) {
    operatorError(
      'RELATIONSHIP_MISMATCH',
      'response.result.definition',
      'diagram read does not match one current definition',
    )
  }
  return Object.freeze({ job, definition, systemArchitecture, processFlow })
}

function parseMutationReceipt(
  value: unknown,
  operation: Extract<StrongFlowOperatorOperation,
    | `review.${string}`
    | 'job.cancel'
    | 'job.resume'>,
): StrongFlowOperatorMutationReceipt {
  const input = record(value, 'response.result', 'INVALID_RESPONSE')
  exactKeys(input, ['job', 'event', 'review'], 'response.result', 'INVALID_RESPONSE')
  const job = parseStrongFlowOperatorJobView(input.job)
  const event = parseStrongFlowOperatorEventView(input.event)
  if (event.jobId !== job.jobId || event.sequence !== job.sequence) {
    operatorError(
      'RELATIONSHIP_MISMATCH',
      'response.result.event',
      'mutation event does not match the returned job snapshot',
    )
  }
  let review: HumanReviewRecord | null = null
  if (input.review !== null) {
    try {
      review = parseStrongFlowArtifactAs('HUMAN_REVIEW_RECORD', input.review)
    } catch (error) {
      operatorError('INVALID_RESPONSE', 'response.result.review', 'mutation review is invalid', {
        cause: error,
      })
    }
  }
  const reviewOperation = operation.startsWith('review.')
  if (reviewOperation !== (review !== null)) {
    operatorError(
      'RELATIONSHIP_MISMATCH',
      'response.result.review',
      'mutation review presence does not match the operation',
    )
  }
  if (review !== null) {
    const expectedDecision = operation === 'review.approve'
      ? 'approved'
      : operation === 'review.reject'
        ? 'rejected'
        : 'changes-requested'
    if (review.jobId !== job.jobId
      || review.payload.decision !== expectedDecision
      || event.kind !== `human-review.${expectedDecision}`) {
      operatorError(
        'RELATIONSHIP_MISMATCH',
        'response.result.review',
        'review receipt does not match the operation and job',
      )
    }
  } else {
    const expectedKind = operation === 'job.cancel' ? 'job.cancelled' : 'job.resumed'
    if (event.kind !== expectedKind) {
      operatorError(
        'RELATIONSHIP_MISMATCH',
        'response.result.event.kind',
        'job mutation event does not match the operation',
      )
    }
  }
  return Object.freeze({ job, event, review })
}

export function parseStrongFlowOperatorArtifactPage(
  value: unknown,
): StrongFlowOperatorArtifactPage {
  const input = record(value, 'response.result', 'INVALID_RESPONSE')
  exactKeys(input, [
    'schemaVersion',
    'jobId',
    'afterSequence',
    'artifacts',
    'nextAfterSequence',
  ], 'response.result', 'INVALID_RESPONSE')
  if (input.schemaVersion !== STRONGFLOW_OPERATOR_ARTIFACT_LINK_SCHEMA_VERSION) {
    operatorError(
      'UNSUPPORTED_SCHEMA_VERSION',
      'response.result.schemaVersion',
      'artifact page schema version is unsupported',
    )
  }
  const jobId = parseJobId(input.jobId, 'response.result.jobId')
  const afterSequence = input.afterSequence === null
    ? null
    : decimalSequence(input.afterSequence, 'response.result.afterSequence')
  if (!Array.isArray(input.artifacts)
    || input.artifacts.length > STRONGFLOW_OPERATOR_MAX_ARTIFACT_PAGE) {
    operatorError(
      'LIMIT_EXCEEDED',
      'response.result.artifacts',
      'artifact page exceeds its product maximum',
    )
  }
  const artifacts = input.artifacts.map(parseStrongFlowOperatorArtifactLink)
  if (artifacts.some(link => link.jobId !== jobId)) {
    operatorError(
      'RELATIONSHIP_MISMATCH',
      'response.result.artifacts',
      'artifact page contains another job',
    )
  }
  for (let index = 1; index < artifacts.length; index += 1) {
    if (BigInt(artifacts[index - 1]!.sequence) >= BigInt(artifacts[index]!.sequence)) {
      operatorError(
        'RELATIONSHIP_MISMATCH',
        `response.result.artifacts[${index}].sequence`,
        'artifact page sequence is not strictly increasing',
      )
    }
  }
  if (afterSequence !== null
    && artifacts.length > 0
    && BigInt(artifacts[0]!.sequence) <= BigInt(afterSequence)) {
    operatorError(
      'RELATIONSHIP_MISMATCH',
      'response.result.artifacts[0].sequence',
      'artifact page did not advance beyond its sequence',
    )
  }
  const nextAfterSequence = input.nextAfterSequence === null
    ? null
    : decimalSequence(input.nextAfterSequence, 'response.result.nextAfterSequence')
  const expectedNext = artifacts.at(-1)?.sequence ?? null
  if (nextAfterSequence !== expectedNext) {
    operatorError(
      'RELATIONSHIP_MISMATCH',
      'response.result.nextAfterSequence',
      'artifact page cursor does not identify its last artifact',
    )
  }
  return Object.freeze({
    schemaVersion: STRONGFLOW_OPERATOR_ARTIFACT_LINK_SCHEMA_VERSION,
    jobId,
    afterSequence,
    artifacts: Object.freeze(artifacts),
    nextAfterSequence,
  })
}

function parseExportManifest(value: unknown): StrongFlowOperatorExportManifest {
  const input = record(value, 'response.result', 'INVALID_RESPONSE')
  exactKeys(
    input,
    ['schemaVersion', 'format', 'exportedAtMillis', 'job', 'events', 'artifacts'],
    'response.result',
    'INVALID_RESPONSE',
  )
  if (input.schemaVersion !== STRONGFLOW_OPERATOR_EXPORT_SCHEMA_VERSION) {
    operatorError(
      'UNSUPPORTED_SCHEMA_VERSION',
      'response.result.schemaVersion',
      'operator export schema version is unsupported',
    )
  }
  if (input.format !== 'manifest-json') {
    operatorError('INVALID_RESPONSE', 'response.result.format', 'operator export format is unsupported')
  }
  const job = parseStrongFlowOperatorJobView(input.job)
  if (!Array.isArray(input.events) || input.events.length > STRONGFLOW_OPERATOR_MAX_EXPORT_EVENTS) {
    operatorError('LIMIT_EXCEEDED', 'response.result.events', 'operator export has too many events')
  }
  if (!Array.isArray(input.artifacts)
    || input.artifacts.length > STRONGFLOW_OPERATOR_MAX_EXPORT_ARTIFACTS) {
    operatorError(
      'LIMIT_EXCEEDED',
      'response.result.artifacts',
      'operator export has too many artifacts',
    )
  }
  const events = input.events.map(parseStrongFlowOperatorEventView)
  const artifacts = input.artifacts.map(parseStrongFlowOperatorArtifactLink)
  if (events.some(event => event.jobId !== job.jobId)
    || artifacts.some(link => link.jobId !== job.jobId)) {
    operatorError(
      'RELATIONSHIP_MISMATCH',
      'response.result',
      'operator export mixes different jobs',
    )
  }
  return Object.freeze({
    schemaVersion: STRONGFLOW_OPERATOR_EXPORT_SCHEMA_VERSION,
    format: 'manifest-json',
    exportedAtMillis: nonNegativeInteger(
      input.exportedAtMillis,
      'response.result.exportedAtMillis',
    ),
    job,
    events: Object.freeze(events),
    artifacts: Object.freeze(artifacts),
  })
}

function parseJobResult(value: unknown): { readonly job: StrongFlowOperatorJobView } {
  const input = record(value, 'response.result', 'INVALID_RESPONSE')
  exactKeys(input, ['job'], 'response.result', 'INVALID_RESPONSE')
  return Object.freeze({ job: parseStrongFlowOperatorJobView(input.job) })
}

function parseResponseResult(
  operation: StrongFlowOperatorOperation,
  value: unknown,
): StrongFlowOperatorResponseResultByOperation[StrongFlowOperatorOperation] {
  switch (operation) {
    case 'job.create':
    case 'job.status': return parseJobResult(value)
    case 'job.follow': return parseStrongFlowOperatorEventPage(value)
    case 'definition.requirement': return parseArtifactReadResult(value, 'REQUIREMENT_SPEC')
    case 'definition.solution': return parseArtifactReadResult(value, 'SOLUTION_DESIGN')
    case 'definition.diagrams': return parseDiagramsResult(value)
    case 'review.approve':
    case 'review.reject':
    case 'review.request-changes':
    case 'job.cancel':
    case 'job.resume': return parseMutationReceipt(value, operation)
    case 'job.artifacts': return parseStrongFlowOperatorArtifactPage(value)
    case 'job.export': return parseExportManifest(value)
  }
}

function parsePublicError(value: unknown): StrongFlowOperatorPublicError {
  const input = record(value, 'response.error', 'INVALID_RESPONSE')
  exactKeys(input, [
    'code',
    'category',
    'status',
    'retryable',
    'message',
    'field',
    'currentDefinition',
  ], 'response.error', 'INVALID_RESPONSE')
  const code = enumValue(
    input.code,
    STRONGFLOW_OPERATOR_ERROR_CODES,
    'response.error.code',
    'INVALID_RESPONSE',
  )
  const definition = STRONGFLOW_OPERATOR_ERROR_DEFINITIONS[code]
  if (input.category !== definition.category
    || input.status !== definition.status
    || input.retryable !== definition.retryable) {
    operatorError(
      'RELATIONSHIP_MISMATCH',
      'response.error',
      'operator error metadata does not match its stable code',
    )
  }
  const currentDefinition = input.currentDefinition === null
    ? null
    : parseDefinition(
      input.currentDefinition,
      'response.error.currentDefinition',
      'INVALID_RESPONSE',
    )
  if ((code === 'STALE_DEFINITION') !== (currentDefinition !== null)) {
    operatorError(
      'RELATIONSHIP_MISMATCH',
      'response.error.currentDefinition',
      'current definition presence does not match the operator error code',
    )
  }
  return Object.freeze({
    code,
    category: definition.category,
    status: definition.status,
    retryable: definition.retryable,
    message: boundedText(input.message, 'response.error.message', 1_000, {
      code: 'INVALID_RESPONSE',
    }),
    field: input.field === null
      ? null
      : boundedText(input.field, 'response.error.field', 500, { code: 'INVALID_RESPONSE' }),
    currentDefinition,
  })
}

export function parseStrongFlowOperatorResponse(value: unknown): StrongFlowOperatorResponse {
  const input = record(value, 'response', 'INVALID_RESPONSE')
  const success = input.ok === true
  exactKeys(
    input,
    success
      ? ['schemaVersion', 'requestId', 'operation', 'ok', 'result']
      : ['schemaVersion', 'requestId', 'operation', 'ok', 'error'],
    'response',
    'INVALID_RESPONSE',
  )
  if (input.schemaVersion !== STRONGFLOW_OPERATOR_SCHEMA_VERSION) {
    operatorError(
      'UNSUPPORTED_SCHEMA_VERSION',
      'response.schemaVersion',
      'operator response schema version is unsupported',
    )
  }
  if (input.ok !== true && input.ok !== false) {
    operatorError('INVALID_RESPONSE', 'response.ok', 'operator response success flag is invalid')
  }
  if (success) {
    const operation = enumValue(
      input.operation,
      STRONGFLOW_OPERATOR_OPERATIONS,
      'response.operation',
      'INVALID_RESPONSE',
    )
    return Object.freeze({
      schemaVersion: STRONGFLOW_OPERATOR_SCHEMA_VERSION,
      requestId: StrongFlowOperatorRequestId(portableIdentifier(
        input.requestId,
        'response.requestId',
      )),
      operation,
      ok: true,
      result: parseResponseResult(operation, input.result),
    }) as StrongFlowOperatorSuccessResponse
  }
  const requestId = input.requestId === null
    ? null
    : StrongFlowOperatorRequestId(portableIdentifier(input.requestId, 'response.requestId'))
  const operation = input.operation === null
    ? null
    : enumValue(
      input.operation,
      STRONGFLOW_OPERATOR_OPERATIONS,
      'response.operation',
      'INVALID_RESPONSE',
    )
  return Object.freeze({
    schemaVersion: STRONGFLOW_OPERATOR_SCHEMA_VERSION,
    requestId,
    operation,
    ok: false,
    error: parsePublicError(input.error),
  })
}

function successJobId(response: StrongFlowOperatorSuccessResponse): JobIdentifier {
  switch (response.operation) {
    case 'job.follow': return response.result.jobId
    case 'job.artifacts': return response.result.jobId
    case 'job.create':
    case 'job.status':
    case 'definition.requirement':
    case 'definition.solution':
    case 'definition.diagrams':
    case 'review.approve':
    case 'review.reject':
    case 'review.request-changes':
    case 'job.cancel':
    case 'job.resume': return response.result.job.jobId
    case 'job.export': return response.result.job.jobId
  }
}

function definitionsEqual(left: DefinitionIdentity, right: DefinitionIdentity): boolean {
  return left.requirementId === right.requirementId
    && left.solutionId === right.solutionId
    && left.systemArchitectureDiagramId === right.systemArchitectureDiagramId
    && left.processFlowDiagramId === right.processFlowDiagramId
}

/**
 * Validates the response against the exact request that caused it. Callers use
 * this after transport so a response cannot switch operation, job, or review target.
 */
export function parseStrongFlowOperatorResponseForRequest<
  Operation extends StrongFlowOperatorOperation,
>(
  requestValue: StrongFlowOperatorRequestFor<Operation>,
  responseValue: unknown,
): StrongFlowOperatorResponseFor<Operation>
export function parseStrongFlowOperatorResponseForRequest(
  requestValue: unknown,
  responseValue: unknown,
): StrongFlowOperatorResponse {
  const request = parseStrongFlowOperatorRequest(requestValue)
  const response = parseStrongFlowOperatorResponse(responseValue)
  if (response.requestId !== request.requestId || response.operation !== request.operation) {
    operatorError(
      'RELATIONSHIP_MISMATCH',
      'response',
      'operator response does not match its request identity and operation',
    )
  }
  if ((request.operation === 'review.approve'
      || request.operation === 'review.reject'
      || request.operation === 'review.request-changes')
    && JSON.stringify(response.ok ? response.result : response.error)
      .includes(request.payload.authentication.proof)) {
    operatorError(
      'INVALID_RESPONSE',
      'response',
      'operator response contains review authentication material',
    )
  }
  if (!response.ok) return response
  if (request.operation !== 'job.create') {
    const expectedJobId = request.payload.jobId
    if (successJobId(response) !== expectedJobId) {
      operatorError(
        'RELATIONSHIP_MISMATCH',
        'response.result',
        'operator response belongs to another job',
      )
    }
  }
  if (request.operation === 'review.approve'
    || request.operation === 'review.reject'
    || request.operation === 'review.request-changes') {
    if (response.operation !== request.operation
      || response.result.review === null
      || !definitionsEqual(response.result.review.payload.definition, request.payload.definition)) {
      operatorError(
        'RELATIONSHIP_MISMATCH',
        'response.result.review',
        'successful review response does not match the submitted definition',
      )
    }
  }
  return response
}

export function materializeStrongFlowOperatorSuccess<Operation extends StrongFlowOperatorOperation>(
  request: StrongFlowOperatorRequestFor<Operation>,
  result: StrongFlowOperatorResponseResultByOperation[Operation],
): StrongFlowOperatorSuccessResponseFor<Operation> {
  return parseStrongFlowOperatorResponseForRequest(request, {
    schemaVersion: STRONGFLOW_OPERATOR_SCHEMA_VERSION,
    requestId: request.requestId,
    operation: request.operation,
    ok: true,
    result,
  }) as StrongFlowOperatorSuccessResponseFor<Operation>
}

export function materializeStrongFlowOperatorFailure(input: {
  readonly requestId: StrongFlowOperatorRequestId | string | null
  readonly operation: StrongFlowOperatorOperation | null
  readonly code: StrongFlowOperatorErrorCode
  readonly message: string
  readonly field?: string | null
  readonly currentDefinition?: DefinitionIdentity | null
}): StrongFlowOperatorFailureResponse {
  const definition = STRONGFLOW_OPERATOR_ERROR_DEFINITIONS[input.code]
  return parseStrongFlowOperatorResponse({
    schemaVersion: STRONGFLOW_OPERATOR_SCHEMA_VERSION,
    requestId: input.requestId,
    operation: input.operation,
    ok: false,
    error: {
      code: input.code,
      category: definition.category,
      status: definition.status,
      retryable: definition.retryable,
      message: input.message,
      field: input.field ?? null,
      currentDefinition: input.currentDefinition ?? null,
    },
  }) as StrongFlowOperatorFailureResponse
}

export interface StrongFlowOperatorInvokeOptions {
  /** Aborting a request disconnects the caller; it never cancels the durable job. */
  readonly signal?: AbortSignal
}

/** One transport-neutral seam shared by the DSH workbench and the CLI adapter. */
export interface StrongFlowOperatorInvoker {
  invoke<Operation extends StrongFlowOperatorOperation>(
    request: StrongFlowOperatorRequestFor<Operation>,
    options?: StrongFlowOperatorInvokeOptions,
  ): Promise<StrongFlowOperatorResponseFor<Operation>>
}

export interface StrongFlowCliCommandDescriptor {
  readonly command: string
  readonly operation: StrongFlowOperatorOperation
  readonly usage: string
  readonly summary: string
  readonly mutatesJob: boolean
  readonly idempotency: 'request-id'
}

export const STRONGFLOW_CLI_COMMANDS: readonly StrongFlowCliCommandDescriptor[] = Object.freeze([
  Object.freeze({
    command: 'create',
    operation: 'job.create',
    usage: 'winwincode create --repo PATH --request TEXT [--base REV] [--title TEXT] --json',
    summary: '创建一个 StrongFlow 作业。',
    mutatesJob: true,
    idempotency: 'request-id',
  }),
  Object.freeze({
    command: 'status',
    operation: 'job.status',
    usage: 'winwincode status JOB_ID --json',
    summary: '读取作业的当前正式快照。',
    mutatesJob: false,
    idempotency: 'request-id',
  }),
  Object.freeze({
    command: 'follow',
    operation: 'job.follow',
    usage: 'winwincode follow JOB_ID [--after CURSOR] [--wait MS] --json-lines',
    summary: '从游标继续读取有序事件；断开不会取消作业。',
    mutatesJob: false,
    idempotency: 'request-id',
  }),
  Object.freeze({
    command: 'requirement',
    operation: 'definition.requirement',
    usage: 'winwincode requirement JOB_ID --json',
    summary: '读取当前需求及其固定制品身份。',
    mutatesJob: false,
    idempotency: 'request-id',
  }),
  Object.freeze({
    command: 'solution',
    operation: 'definition.solution',
    usage: 'winwincode solution JOB_ID --json',
    summary: '读取与当前需求分开的方案及其身份。',
    mutatesJob: false,
    idempotency: 'request-id',
  }),
  Object.freeze({
    command: 'diagrams',
    operation: 'definition.diagrams',
    usage: 'winwincode diagrams JOB_ID --json',
    summary: '读取当前系统架构图和流程图。',
    mutatesJob: false,
    idempotency: 'request-id',
  }),
  Object.freeze({
    command: 'approve',
    operation: 'review.approve',
    usage: 'winwincode approve JOB_ID --definition FILE --auth PROOF [--comment TEXT] --json',
    summary: '只批准 FILE 中四个完全匹配的当前定义身份。',
    mutatesJob: true,
    idempotency: 'request-id',
  }),
  Object.freeze({
    command: 'reject',
    operation: 'review.reject',
    usage: 'winwincode reject JOB_ID --definition FILE --auth PROOF [--comment TEXT] --json',
    summary: '拒绝 FILE 中标识的当前定义。',
    mutatesJob: true,
    idempotency: 'request-id',
  }),
  Object.freeze({
    command: 'request-changes',
    operation: 'review.request-changes',
    usage: 'winwincode request-changes JOB_ID --definition FILE --scope SCOPE --auth PROOF [--comment TEXT] --json',
    summary: '要求修改需求、方案或两张图。',
    mutatesJob: true,
    idempotency: 'request-id',
  }),
  Object.freeze({
    command: 'cancel',
    operation: 'job.cancel',
    usage: 'winwincode cancel JOB_ID --reason TEXT --json',
    summary: '显式取消一个非终态作业。',
    mutatesJob: true,
    idempotency: 'request-id',
  }),
  Object.freeze({
    command: 'resume',
    operation: 'job.resume',
    usage: 'winwincode resume JOB_ID --interruption-sequence SEQUENCE --json',
    summary: '从准确的中断序号恢复作业。',
    mutatesJob: true,
    idempotency: 'request-id',
  }),
  Object.freeze({
    command: 'artifacts',
    operation: 'job.artifacts',
    usage: 'winwincode artifacts JOB_ID [--after-sequence SEQUENCE] [--kind KIND] --json',
    summary: '按连续序号列出制品链接。',
    mutatesJob: false,
    idempotency: 'request-id',
  }),
  Object.freeze({
    command: 'export',
    operation: 'job.export',
    usage: 'winwincode export JOB_ID --format manifest-json --json',
    summary: '导出不含凭据和私有运行时对象的作业清单。',
    mutatesJob: false,
    idempotency: 'request-id',
  }),
])

export const STRONGFLOW_CLI_EXIT_CODES = Object.freeze({
  success: 0,
  serviceFailure: 1,
  usage: 2,
  notFound: 3,
  conflict: 4,
  authentication: 5,
  sigint: 130,
  sigterm: 143,
} as const)

export type StrongFlowCliSignal = 'SIGINT' | 'SIGTERM'

export function strongFlowCliSignalExitCode(signal: StrongFlowCliSignal): 130 | 143 {
  return signal === 'SIGINT'
    ? STRONGFLOW_CLI_EXIT_CODES.sigint
    : STRONGFLOW_CLI_EXIT_CODES.sigterm
}

export function strongFlowCliExitCode(response: StrongFlowOperatorResponse): number {
  if (response.ok) return STRONGFLOW_CLI_EXIT_CODES.success
  return STRONGFLOW_OPERATOR_ERROR_DEFINITIONS[response.error.code].exitCode
}

export function renderStrongFlowCliHelp(): string {
  const commands = STRONGFLOW_CLI_COMMANDS.map(command => (
    `  ${command.usage}\n      ${command.summary}`
  )).join('\n')
  return [
    'WinWinCode StrongFlow commands',
    '',
    'Every command accepts --request-id ID. Mutating retries with the same ID return the first result.',
    'JSON success and error envelopes use schemaVersion 1.',
    'SIGINT exits 130 and SIGTERM exits 143; either signal stops only this CLI request.',
    'Use the cancel command to cancel the durable job.',
    '',
    commands,
    '',
  ].join('\n')
}
