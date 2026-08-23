import {
  DELIVERY_STAGES,
  AttentionItemId,
  DeliveryId,
  DeliveryTaskId,
  EvidenceRefId,
  SessionBindingId,
  StageRunId,
  parseAttentionItem,
  parseDelivery,
  parseDeliverySpec,
  parseDeliveryTask,
  type AttentionItem,
  type Delivery,
  type DeliveryId as DeliveryIdentifier,
  type DeliverySpec,
  type DeliveryStage,
  type DeliveryTask,
  type DeliveryTaskId as DeliveryTaskIdentifier,
  type AttentionItemId as AttentionItemIdentifier,
  type EvidenceRefId as EvidenceRefIdentifier,
  type SessionBindingId as SessionBindingIdentifier,
  type StageRunActorType,
  type StageRunId as StageRunIdentifier,
} from './delivery.js'
import {
  parseFrozenDeliveryCandidate,
  type FrozenDeliveryCandidate,
} from './delivery-candidate.js'
import type { RuntimeEvent } from './runtime-events.js'
import {
  STRONGFLOW_VERIFICATION_ROLE_IDS,
  type StrongFlowVerificationRoleId,
} from './strongflow-role.js'
import {
  parseStrongFlowDiagramExecutionProjection,
  type StrongFlowDiagramExecutionProjection,
} from './strongflow-diagram-execution.js'

export const STRONGFLOW_DELIVERY_API_SCHEMA_VERSION = 6 as const

export const STRONGFLOW_DELIVERY_REMEDIATION_PROTOCOL =
  'winwincode.delivery-remediation.v1' as const

export const STRONGFLOW_REMEDIATION_DIAGRAM_KINDS = Object.freeze([
  'system-architecture',
  'process-flow',
] as const)

export type StrongFlowRemediationDiagramKind =
  typeof STRONGFLOW_REMEDIATION_DIAGRAM_KINDS[number]

export const STRONGFLOW_DELIVERY_OPERATIONS = Object.freeze([
  'createDelivery',
  'updateDeliverySpec',
  'startStage',
  'bindSession',
  'resolveAttention',
  'submitVerdict',
  'getDeliveryProjection',
] as const)

export type StrongFlowDeliveryOperation = typeof STRONGFLOW_DELIVERY_OPERATIONS[number]

export interface StrongFlowCreateDeliveryPayload {
  readonly spec: DeliverySpec
  readonly tasks: readonly DeliveryTask[]
}

export interface StrongFlowUpdateDeliverySpecPayload {
  readonly deliveryId: DeliveryIdentifier
  readonly expectedRevision: number
  readonly spec: DeliverySpec
}

export interface StrongFlowStartStagePayload {
  readonly deliveryId: DeliveryIdentifier
  readonly expectedRevision: number
  readonly stageRunId: StageRunIdentifier
  readonly deliveryTaskId: DeliveryTaskIdentifier | null
  readonly stage: DeliveryStage
  readonly actorType: StageRunActorType
  readonly role: string
  readonly attention: AttentionItem | null
}

export interface StrongFlowBindSessionPayload {
  readonly deliveryId: DeliveryIdentifier
  readonly expectedRevision: number
  readonly bindingId: SessionBindingIdentifier
  readonly stageRunId: StageRunIdentifier
  readonly dshSessionId: string | null
  readonly codexSessionId: string | null
}

export type StrongFlowDeliveryChannel = 'local-ui' | 'cli'

export type StrongFlowDeliveryAuthentication =
  | {
    readonly scheme: 'local-session'
    readonly proof: string
  }
  | {
    readonly scheme: 'local-peer'
    readonly proof: string
  }

export interface StrongFlowResolveAttentionPayload {
  readonly deliveryId: DeliveryIdentifier
  readonly expectedRevision: number
  readonly attentionItemId: AttentionItemIdentifier
  readonly status: 'resolved' | 'dismissed'
  readonly resolution: string
  readonly remediation: StrongFlowDeliveryRemediation | null
  readonly channel: StrongFlowDeliveryChannel
  readonly authentication: StrongFlowDeliveryAuthentication
}

/** Request-only diagram annotation. It is serialized inside AttentionItem.resolution. */
export interface StrongFlowRemediationAnnotation {
  readonly schemaVersion: typeof STRONGFLOW_DELIVERY_API_SCHEMA_VERSION
  readonly id: string
  readonly diagramKind: StrongFlowRemediationDiagramKind
  readonly diagramId: string
  readonly nodeId: string
  readonly filePath: string
  readonly hunkSha256: string
  readonly evidenceRefIds: readonly EvidenceRefIdentifier[]
  readonly note: string
}

/** Request-only rework command; no additional persisted business object is created. */
export interface StrongFlowDeliveryRemediation {
  readonly schemaVersion: typeof STRONGFLOW_DELIVERY_API_SCHEMA_VERSION
  readonly protocol: typeof STRONGFLOW_DELIVERY_REMEDIATION_PROTOCOL
  readonly deliveryTaskId: DeliveryTaskIdentifier
  readonly candidate: FrozenDeliveryCandidate
  readonly annotations: readonly StrongFlowRemediationAnnotation[]
}

export interface StrongFlowSubmitVerdictPayload {
  readonly deliveryId: DeliveryIdentifier
  readonly expectedRevision: number
  readonly candidate: FrozenDeliveryCandidate
  readonly runtimeEvents: readonly RuntimeEvent[]
  readonly requiredRoles: readonly StrongFlowVerificationRoleId[]
}

export interface StrongFlowGetDeliveryProjectionPayload {
  readonly deliveryId: DeliveryIdentifier
}

export interface StrongFlowDeliveryPayloadByOperation {
  readonly createDelivery: StrongFlowCreateDeliveryPayload
  readonly updateDeliverySpec: StrongFlowUpdateDeliverySpecPayload
  readonly startStage: StrongFlowStartStagePayload
  readonly bindSession: StrongFlowBindSessionPayload
  readonly resolveAttention: StrongFlowResolveAttentionPayload
  readonly submitVerdict: StrongFlowSubmitVerdictPayload
  readonly getDeliveryProjection: StrongFlowGetDeliveryProjectionPayload
}

export type StrongFlowDeliveryRequestFor<Operation extends StrongFlowDeliveryOperation> = {
  readonly schemaVersion: typeof STRONGFLOW_DELIVERY_API_SCHEMA_VERSION
  readonly requestId: string
  readonly operation: Operation
  readonly payload: StrongFlowDeliveryPayloadByOperation[Operation]
}

export type StrongFlowDeliveryRequest = {
  readonly [Operation in StrongFlowDeliveryOperation]: StrongFlowDeliveryRequestFor<Operation>
}[StrongFlowDeliveryOperation]

export const STRONGFLOW_DELIVERY_ERROR_CODES = Object.freeze([
  'INVALID_SERVICE_OPTIONS',
  'INVALID_REQUEST',
  'DELIVERY_NOT_FOUND',
  'DELIVERY_CONFLICT',
  'REVISION_CONFLICT',
  'WRONG_DELIVERY_STATE',
  'ATTENTION_REQUIRED',
  'AUTHENTICATION_REQUIRED',
  'AUTHENTICATION_FAILED',
  'STORE_FAILURE',
  'OPERATION_ABORTED',
  'INTERNAL_ERROR',
] as const)

export type StrongFlowDeliveryErrorCode = typeof STRONGFLOW_DELIVERY_ERROR_CODES[number]

export interface StrongFlowDeliveryPublicError {
  readonly code: StrongFlowDeliveryErrorCode
  readonly message: string
  readonly currentRevision: number | null
}

export interface StrongFlowDeliverySuccessResponse<
  Operation extends StrongFlowDeliveryOperation = StrongFlowDeliveryOperation,
> {
  readonly schemaVersion: typeof STRONGFLOW_DELIVERY_API_SCHEMA_VERSION
  readonly requestId: string
  readonly operation: Operation
  readonly ok: true
  readonly result: {
    readonly delivery: Delivery
    readonly diagramExecution: StrongFlowDiagramExecutionProjection | null
  }
}

export interface StrongFlowDeliveryFailureResponse {
  readonly schemaVersion: typeof STRONGFLOW_DELIVERY_API_SCHEMA_VERSION
  readonly requestId: string | null
  readonly operation: StrongFlowDeliveryOperation | null
  readonly ok: false
  readonly error: StrongFlowDeliveryPublicError
}

export type StrongFlowDeliveryResponse =
  | StrongFlowDeliverySuccessResponse
  | StrongFlowDeliveryFailureResponse

export type StrongFlowDeliveryResponseFor<Operation extends StrongFlowDeliveryOperation> =
  | StrongFlowDeliverySuccessResponse<Operation>
  | StrongFlowDeliveryFailureResponse

export type StrongFlowDeliveryApiValidationErrorCode =
  | 'INVALID_REQUEST'
  | 'INVALID_RESPONSE'
  | 'UNSUPPORTED_SCHEMA_VERSION'
  | 'RELATIONSHIP_MISMATCH'

export class StrongFlowDeliveryApiValidationError extends Error {
  readonly code: StrongFlowDeliveryApiValidationErrorCode
  readonly path: string

  constructor(
    code: StrongFlowDeliveryApiValidationErrorCode,
    path: string,
    message: string,
    options?: ErrorOptions,
  ) {
    super(message, options)
    this.name = 'StrongFlowDeliveryApiValidationError'
    this.code = code
    this.path = path
  }
}

const REQUEST_ID_PATTERN = /^[A-Za-z0-9][A-Za-z0-9._:/@-]{0,499}$/u
const SESSION_ID_PATTERN = /^[A-Za-z0-9][A-Za-z0-9._:/@-]{0,199}$/u
const MAX_TEXT_LENGTH = 65_536
const MAX_RUNTIME_EVENTS = 65_536
const MAX_RUNTIME_EVENT_JSON_LENGTH = 16 * 1_024 * 1_024
const MAX_REMEDIATION_ANNOTATIONS = 100
const MAX_REMEDIATION_ANNOTATION_JSON_LENGTH = 48 * 1_024
const MAX_REMEDIATION_EVIDENCE_REFS = 32

function apiError(
  code: StrongFlowDeliveryApiValidationErrorCode,
  path: string,
  message: string,
  options?: ErrorOptions,
): never {
  throw new StrongFlowDeliveryApiValidationError(code, path, message, options)
}

function isRecord(value: unknown): value is Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return false
  const prototype = Object.getPrototypeOf(value)
  return prototype === Object.prototype || prototype === null
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

function record(
  value: unknown,
  path: string,
  code: 'INVALID_REQUEST' | 'INVALID_RESPONSE',
): Record<string, unknown> {
  if (!isRecord(value)) apiError(code, path, `${path} must be an object`)
  return value
}

function exactKeys(
  value: Readonly<Record<string, unknown>>,
  expected: readonly string[],
  path: string,
  code: 'INVALID_REQUEST' | 'INVALID_RESPONSE',
): void {
  const keys = new Set(expected)
  if (Object.keys(value).length !== keys.size
    || expected.some(key => !Object.hasOwn(value, key))
    || Object.keys(value).some(key => !keys.has(key))) {
    apiError(code, path, `${path} has an unexpected shape`)
  }
}

function operation(
  value: unknown,
  path: string,
  code: 'INVALID_REQUEST' | 'INVALID_RESPONSE' = 'INVALID_REQUEST',
): StrongFlowDeliveryOperation {
  if (typeof value !== 'string'
    || !STRONGFLOW_DELIVERY_OPERATIONS.includes(value as StrongFlowDeliveryOperation)) {
    apiError(code, path, `${path} is unsupported`)
  }
  return value as StrongFlowDeliveryOperation
}

function portableRequestId(
  value: unknown,
  path: string,
  code: 'INVALID_REQUEST' | 'INVALID_RESPONSE' = 'INVALID_REQUEST',
): string {
  if (typeof value !== 'string' || !REQUEST_ID_PATTERN.test(value)) {
    apiError(code, path, `${path} is invalid`)
  }
  return value
}

function portableSessionId(value: unknown, path: string): string | null {
  if (value === null) return null
  if (typeof value !== 'string' || !SESSION_ID_PATTERN.test(value)) {
    apiError('INVALID_REQUEST', path, `${path} is invalid`)
  }
  return value
}

function boundedText(
  value: unknown,
  path: string,
  code: 'INVALID_REQUEST' | 'INVALID_RESPONSE' = 'INVALID_REQUEST',
): string {
  if (typeof value !== 'string'
    || value.trim().length === 0
    || value.length > MAX_TEXT_LENGTH
    || /[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f]/u.test(value)) {
    apiError(code, path, `${path} must be non-empty bounded text`)
  }
  return value
}

function revision(
  value: unknown,
  path: string,
  code: 'INVALID_REQUEST' | 'INVALID_RESPONSE' = 'INVALID_REQUEST',
): number {
  if (!Number.isSafeInteger(value) || Number(value) < 1) {
    apiError(code, path, `${path} must be a positive safe integer`)
  }
  return Number(value)
}

function remediationIdentifier(value: unknown, path: string): string {
  if (typeof value !== 'string' || !SESSION_ID_PATTERN.test(value)) {
    apiError('INVALID_REQUEST', path, `${path} is invalid`)
  }
  return value
}

function remediationFilePath(value: unknown, path: string): string {
  const filePath = boundedText(value, path)
  const segments = filePath.split('/')
  if (filePath.length > 4_096
    || filePath.startsWith('/')
    || filePath.includes('\\')
    || segments.some(segment => segment.length === 0 || segment === '.' || segment === '..')) {
    apiError('INVALID_REQUEST', path, `${path} must be a repository-relative file path`)
  }
  return filePath
}

function remediationSha256(value: unknown, path: string): string {
  if (typeof value !== 'string' || !/^[a-f0-9]{64}$/u.test(value)) {
    apiError('INVALID_REQUEST', path, `${path} must be a lowercase SHA-256 digest`)
  }
  return value
}

/** Parse the request-only annotation bundle that authorizes one bounded rework. */
export function parseStrongFlowDeliveryRemediation(
  value: unknown,
  path = 'deliveryRemediation',
): StrongFlowDeliveryRemediation {
  const input = record(value, path, 'INVALID_REQUEST')
  exactKeys(input, [
    'schemaVersion',
    'protocol',
    'deliveryTaskId',
    'candidate',
    'annotations',
  ], path, 'INVALID_REQUEST')
  if (input.schemaVersion !== STRONGFLOW_DELIVERY_API_SCHEMA_VERSION
    || input.protocol !== STRONGFLOW_DELIVERY_REMEDIATION_PROTOCOL) {
    apiError('INVALID_REQUEST', path, `${path} protocol is unsupported`)
  }
  if (!Array.isArray(input.annotations)
    || input.annotations.length === 0
    || input.annotations.length > MAX_REMEDIATION_ANNOTATIONS) {
    apiError('INVALID_REQUEST', `${path}.annotations`, 'annotations must be a bounded non-empty array')
  }
  let annotationJson: string
  try {
    annotationJson = JSON.stringify(input.annotations)
  } catch (error) {
    apiError(
      'INVALID_REQUEST',
      `${path}.annotations`,
      'annotations must be JSON serializable',
      { cause: error },
    )
  }
  if (annotationJson.length > MAX_REMEDIATION_ANNOTATION_JSON_LENGTH) {
    apiError('INVALID_REQUEST', `${path}.annotations`, 'annotations exceed the request size limit')
  }
  const annotationIds = new Set<string>()
  const annotations = input.annotations.map((value, index): StrongFlowRemediationAnnotation => {
    const annotationPath = `${path}.annotations[${String(index)}]`
    const annotation = record(value, annotationPath, 'INVALID_REQUEST')
    exactKeys(annotation, [
      'schemaVersion',
      'id',
      'diagramKind',
      'diagramId',
      'nodeId',
      'filePath',
      'hunkSha256',
      'evidenceRefIds',
      'note',
    ], annotationPath, 'INVALID_REQUEST')
    if (annotation.schemaVersion !== STRONGFLOW_DELIVERY_API_SCHEMA_VERSION) {
      apiError('INVALID_REQUEST', `${annotationPath}.schemaVersion`, 'annotation schemaVersion is unsupported')
    }
    if (typeof annotation.diagramKind !== 'string'
      || !STRONGFLOW_REMEDIATION_DIAGRAM_KINDS.includes(
        annotation.diagramKind as StrongFlowRemediationDiagramKind,
      )) {
      apiError('INVALID_REQUEST', `${annotationPath}.diagramKind`, 'diagramKind is unsupported')
    }
    const id = remediationIdentifier(annotation.id, `${annotationPath}.id`)
    if (annotationIds.has(id)) {
      apiError('INVALID_REQUEST', `${annotationPath}.id`, 'annotation identity is duplicated')
    }
    annotationIds.add(id)
    if (!Array.isArray(annotation.evidenceRefIds)
      || annotation.evidenceRefIds.length === 0
      || annotation.evidenceRefIds.length > MAX_REMEDIATION_EVIDENCE_REFS) {
      apiError(
        'INVALID_REQUEST',
        `${annotationPath}.evidenceRefIds`,
        'annotation evidence references must be a bounded non-empty array',
      )
    }
    const evidenceRefIds = annotation.evidenceRefIds.map((entry, evidenceIndex) => {
      try {
        return EvidenceRefId(remediationIdentifier(
          entry,
          `${annotationPath}.evidenceRefIds[${String(evidenceIndex)}]`,
        ))
      } catch (error) {
        apiError(
          'INVALID_REQUEST',
          `${annotationPath}.evidenceRefIds[${String(evidenceIndex)}]`,
          'annotation evidence reference is invalid',
          { cause: error },
        )
      }
    })
    if (new Set(evidenceRefIds).size !== evidenceRefIds.length) {
      apiError(
        'INVALID_REQUEST',
        `${annotationPath}.evidenceRefIds`,
        'annotation evidence references contain duplicates',
      )
    }
    return Object.freeze({
      schemaVersion: STRONGFLOW_DELIVERY_API_SCHEMA_VERSION,
      id,
      diagramKind: annotation.diagramKind as StrongFlowRemediationDiagramKind,
      diagramId: remediationIdentifier(annotation.diagramId, `${annotationPath}.diagramId`),
      nodeId: remediationIdentifier(annotation.nodeId, `${annotationPath}.nodeId`),
      filePath: remediationFilePath(annotation.filePath, `${annotationPath}.filePath`),
      hunkSha256: remediationSha256(annotation.hunkSha256, `${annotationPath}.hunkSha256`),
      evidenceRefIds: Object.freeze(evidenceRefIds),
      note: boundedText(annotation.note, `${annotationPath}.note`),
    })
  })
  let deliveryTaskId: DeliveryTaskIdentifier
  try {
    deliveryTaskId = DeliveryTaskId(
      remediationIdentifier(input.deliveryTaskId, `${path}.deliveryTaskId`),
    )
  } catch (error) {
    apiError('INVALID_REQUEST', `${path}.deliveryTaskId`, 'deliveryTaskId is invalid', {
      cause: error,
    })
  }
  return immutable({
    schemaVersion: STRONGFLOW_DELIVERY_API_SCHEMA_VERSION,
    protocol: STRONGFLOW_DELIVERY_REMEDIATION_PROTOCOL,
    deliveryTaskId,
    candidate: parseFrozenDeliveryCandidate(input.candidate, `${path}.candidate`),
    annotations: Object.freeze(annotations),
  })
}

function parseAuthentication(
  value: unknown,
  channel: StrongFlowDeliveryChannel,
): StrongFlowDeliveryAuthentication {
  const path = 'request.payload.authentication'
  const input = record(value, path, 'INVALID_REQUEST')
  exactKeys(input, ['scheme', 'proof'], path, 'INVALID_REQUEST')
  if (input.scheme !== 'local-session' && input.scheme !== 'local-peer') {
    apiError('INVALID_REQUEST', `${path}.scheme`, 'authentication scheme is unsupported')
  }
  if ((channel === 'local-ui') !== (input.scheme === 'local-session')) {
    apiError(
      'INVALID_REQUEST',
      `${path}.scheme`,
      'authentication scheme does not match the caller channel',
    )
  }
  const proof = boundedText(input.proof, `${path}.proof`)
  if (proof.length < 16) {
    apiError('INVALID_REQUEST', `${path}.proof`, 'authentication proof is too short')
  }
  return Object.freeze({
    scheme: input.scheme,
    proof,
  })
}

function parseResponseDelivery(value: unknown): Delivery {
  try {
    return parseDelivery(value, 'response.result.delivery')
  } catch (error) {
    apiError(
      'INVALID_RESPONSE',
      'response.result.delivery',
      'response delivery is invalid',
      { cause: error },
    )
  }
}

function parseResponseDiagramExecution(
  value: unknown,
): StrongFlowDiagramExecutionProjection {
  try {
    return parseStrongFlowDiagramExecutionProjection(
      value,
      'response.result.diagramExecution',
    )
  } catch (error) {
    apiError(
      'INVALID_RESPONSE',
      'response.result.diagramExecution',
      'response diagram execution projection is invalid',
      { cause: error },
    )
  }
}

function responseErrorCode(value: unknown): StrongFlowDeliveryErrorCode {
  if (typeof value !== 'string'
    || !STRONGFLOW_DELIVERY_ERROR_CODES.includes(value as StrongFlowDeliveryErrorCode)) {
    apiError('INVALID_RESPONSE', 'response.error.code', 'response error code is unsupported')
  }
  return value as StrongFlowDeliveryErrorCode
}

function parseDeliveryId(value: unknown, path: string): DeliveryIdentifier {
  try {
    if (typeof value !== 'string') throw new Error('delivery id must be a string')
    return DeliveryId(value)
  } catch (error) {
    apiError('INVALID_REQUEST', path, `${path} is invalid`, { cause: error })
  }
}

function parsePayload(
  selectedOperation: StrongFlowDeliveryOperation,
  value: unknown,
): StrongFlowDeliveryPayloadByOperation[StrongFlowDeliveryOperation] {
  const path = 'request.payload'
  const input = record(value, path, 'INVALID_REQUEST')
  try {
    switch (selectedOperation) {
      case 'createDelivery': {
        exactKeys(input, ['spec', 'tasks'], path, 'INVALID_REQUEST')
        if (!Array.isArray(input.tasks) || input.tasks.length > 1_000) {
          apiError('INVALID_REQUEST', `${path}.tasks`, 'tasks must be a bounded array')
        }
        return Object.freeze({
          spec: parseDeliverySpec(input.spec, `${path}.spec`),
          tasks: Object.freeze(input.tasks.map((task, index) => (
            parseDeliveryTask(task, `${path}.tasks[${String(index)}]`)
          ))),
        })
      }
      case 'updateDeliverySpec': {
        exactKeys(input, ['deliveryId', 'expectedRevision', 'spec'], path, 'INVALID_REQUEST')
        return Object.freeze({
          deliveryId: parseDeliveryId(input.deliveryId, `${path}.deliveryId`),
          expectedRevision: revision(input.expectedRevision, `${path}.expectedRevision`),
          spec: parseDeliverySpec(input.spec, `${path}.spec`),
        })
      }
      case 'startStage': {
        exactKeys(input, [
          'deliveryId',
          'expectedRevision',
          'stageRunId',
          'deliveryTaskId',
          'stage',
          'actorType',
          'role',
          'attention',
        ], path, 'INVALID_REQUEST')
        if (typeof input.stage !== 'string'
          || !DELIVERY_STAGES.includes(input.stage as DeliveryStage)) {
          apiError('INVALID_REQUEST', `${path}.stage`, 'stage is unsupported')
        }
        if (input.actorType !== 'codex' && input.actorType !== 'human') {
          apiError('INVALID_REQUEST', `${path}.actorType`, 'actorType is unsupported')
        }
        return Object.freeze({
          deliveryId: parseDeliveryId(input.deliveryId, `${path}.deliveryId`),
          expectedRevision: revision(input.expectedRevision, `${path}.expectedRevision`),
          stageRunId: StageRunId(boundedText(input.stageRunId, `${path}.stageRunId`)),
          deliveryTaskId: input.deliveryTaskId === null
            ? null
            : DeliveryTaskId(boundedText(input.deliveryTaskId, `${path}.deliveryTaskId`)),
          stage: input.stage as DeliveryStage,
          actorType: input.actorType,
          role: boundedText(input.role, `${path}.role`),
          attention: input.attention === null
            ? null
            : parseAttentionItem(input.attention, `${path}.attention`),
        })
      }
      case 'bindSession': {
        exactKeys(input, [
          'deliveryId',
          'expectedRevision',
          'bindingId',
          'stageRunId',
          'dshSessionId',
          'codexSessionId',
        ], path, 'INVALID_REQUEST')
        const dshSessionId = portableSessionId(input.dshSessionId, `${path}.dshSessionId`)
        const codexSessionId = portableSessionId(input.codexSessionId, `${path}.codexSessionId`)
        if (dshSessionId === null && codexSessionId === null) {
          apiError('INVALID_REQUEST', path, 'session binding has no session identity')
        }
        return Object.freeze({
          deliveryId: parseDeliveryId(input.deliveryId, `${path}.deliveryId`),
          expectedRevision: revision(input.expectedRevision, `${path}.expectedRevision`),
          bindingId: SessionBindingId(boundedText(input.bindingId, `${path}.bindingId`)),
          stageRunId: StageRunId(boundedText(input.stageRunId, `${path}.stageRunId`)),
          dshSessionId,
          codexSessionId,
        })
      }
      case 'resolveAttention': {
        exactKeys(input, [
          'deliveryId',
          'expectedRevision',
          'attentionItemId',
          'status',
          'resolution',
          'remediation',
          'channel',
          'authentication',
        ], path, 'INVALID_REQUEST')
        if (input.status !== 'resolved' && input.status !== 'dismissed') {
          apiError('INVALID_REQUEST', `${path}.status`, 'attention status is unsupported')
        }
        if (input.channel !== 'local-ui' && input.channel !== 'cli') {
          apiError('INVALID_REQUEST', `${path}.channel`, 'channel is unsupported')
        }
        return Object.freeze({
          deliveryId: parseDeliveryId(input.deliveryId, `${path}.deliveryId`),
          expectedRevision: revision(input.expectedRevision, `${path}.expectedRevision`),
          attentionItemId: AttentionItemId(
            boundedText(input.attentionItemId, `${path}.attentionItemId`),
          ),
          status: input.status,
          resolution: boundedText(input.resolution, `${path}.resolution`),
          remediation: input.remediation === null
            ? null
            : parseStrongFlowDeliveryRemediation(
              input.remediation,
              `${path}.remediation`,
            ),
          channel: input.channel,
          authentication: parseAuthentication(input.authentication, input.channel),
        })
      }
      case 'submitVerdict': {
        exactKeys(input, [
          'deliveryId',
          'expectedRevision',
          'candidate',
          'runtimeEvents',
          'requiredRoles',
        ], path, 'INVALID_REQUEST')
        if (!Array.isArray(input.runtimeEvents)
          || input.runtimeEvents.length > MAX_RUNTIME_EVENTS
          || input.runtimeEvents.some(event => !isRecord(event))) {
          apiError(
            'INVALID_REQUEST',
            `${path}.runtimeEvents`,
            'runtimeEvents must be a bounded object array',
          )
        }
        let runtimeEventJson: string
        try {
          runtimeEventJson = JSON.stringify(input.runtimeEvents)
        } catch (error) {
          apiError(
            'INVALID_REQUEST',
            `${path}.runtimeEvents`,
            'runtimeEvents must be JSON serializable',
            { cause: error },
          )
        }
        if (runtimeEventJson.length > MAX_RUNTIME_EVENT_JSON_LENGTH) {
          apiError(
            'INVALID_REQUEST',
            `${path}.runtimeEvents`,
            'runtimeEvents exceed the request size limit',
          )
        }
        const requiredRoles = input.requiredRoles
        if (!Array.isArray(requiredRoles)
          || requiredRoles.length < 2
          || requiredRoles.length > STRONGFLOW_VERIFICATION_ROLE_IDS.length
          || requiredRoles.some(role => (
            typeof role !== 'string'
            || !STRONGFLOW_VERIFICATION_ROLE_IDS.includes(
              role as StrongFlowVerificationRoleId,
            )
          ))
          || new Set(requiredRoles).size !== requiredRoles.length
          || !requiredRoles.includes('reviewer')
          || !requiredRoles.includes('verifier')) {
          apiError(
            'INVALID_REQUEST',
            `${path}.requiredRoles`,
            'requiredRoles must contain reviewer and verifier exactly once',
          )
        }
        return Object.freeze({
          deliveryId: parseDeliveryId(input.deliveryId, `${path}.deliveryId`),
          expectedRevision: revision(input.expectedRevision, `${path}.expectedRevision`),
          candidate: parseFrozenDeliveryCandidate(input.candidate, `${path}.candidate`),
          runtimeEvents: immutable(input.runtimeEvents as RuntimeEvent[]),
          requiredRoles: Object.freeze(
            STRONGFLOW_VERIFICATION_ROLE_IDS.filter(role => requiredRoles.includes(role)),
          ),
        })
      }
      case 'getDeliveryProjection': {
        exactKeys(input, ['deliveryId'], path, 'INVALID_REQUEST')
        return Object.freeze({
          deliveryId: parseDeliveryId(input.deliveryId, `${path}.deliveryId`),
        })
      }
    }
  } catch (error) {
    if (error instanceof StrongFlowDeliveryApiValidationError) throw error
    apiError('INVALID_REQUEST', path, `${selectedOperation} payload is invalid`, { cause: error })
  }
}

export function parseStrongFlowDeliveryRequest(value: unknown): StrongFlowDeliveryRequest {
  const input = record(value, 'request', 'INVALID_REQUEST')
  exactKeys(input, ['schemaVersion', 'requestId', 'operation', 'payload'], 'request', 'INVALID_REQUEST')
  if (input.schemaVersion !== STRONGFLOW_DELIVERY_API_SCHEMA_VERSION) {
    apiError(
      'UNSUPPORTED_SCHEMA_VERSION',
      'request.schemaVersion',
      'request schemaVersion is unsupported',
    )
  }
  const selectedOperation = operation(input.operation, 'request.operation')
  return Object.freeze({
    schemaVersion: STRONGFLOW_DELIVERY_API_SCHEMA_VERSION,
    requestId: portableRequestId(input.requestId, 'request.requestId'),
    operation: selectedOperation,
    payload: parsePayload(selectedOperation, input.payload),
  }) as StrongFlowDeliveryRequest
}

export function materializeStrongFlowDeliveryRequest<
  Operation extends StrongFlowDeliveryOperation,
>(
  operationValue: Operation,
  requestIdValue: string,
  payload: StrongFlowDeliveryPayloadByOperation[Operation],
): StrongFlowDeliveryRequestFor<Operation> {
  return parseStrongFlowDeliveryRequest({
    schemaVersion: STRONGFLOW_DELIVERY_API_SCHEMA_VERSION,
    requestId: requestIdValue,
    operation: operationValue,
    payload,
  }) as StrongFlowDeliveryRequestFor<Operation>
}

function requestDeliveryId(request: StrongFlowDeliveryRequest): DeliveryIdentifier {
  return request.operation === 'createDelivery'
    ? request.payload.spec.deliveryId
    : request.payload.deliveryId
}

function containsText(value: unknown, needle: string, seen = new WeakSet<object>()): boolean {
  if (typeof value === 'string') return value.includes(needle)
  if (typeof value !== 'object' || value === null) return false
  if (seen.has(value)) return false
  seen.add(value)
  if (Array.isArray(value)) return value.some(entry => containsText(entry, needle, seen))
  return Object.values(value).some(entry => containsText(entry, needle, seen))
}

export function materializeStrongFlowDeliverySuccess<
  Operation extends StrongFlowDeliveryOperation,
>(
  request: StrongFlowDeliveryRequestFor<Operation>,
  deliveryValue: Delivery,
  diagramExecutionValue: StrongFlowDiagramExecutionProjection | null = null,
): StrongFlowDeliverySuccessResponse<Operation> {
  const delivery = parseResponseDelivery(deliveryValue)
  if (delivery.id !== requestDeliveryId(request as StrongFlowDeliveryRequest)) {
    apiError(
      'RELATIONSHIP_MISMATCH',
      'response.result.delivery.id',
      'response delivery does not match the request',
    )
  }
  const diagramExecution = diagramExecutionValue === null
    ? null
    : parseResponseDiagramExecution(diagramExecutionValue)
  if (diagramExecution !== null
    && (diagramExecution.deliveryId !== delivery.id
      || diagramExecution.deliveryRevision !== delivery.revision)) {
    apiError(
      'RELATIONSHIP_MISMATCH',
      'response.result.diagramExecution',
      'response diagram execution projection does not match the Delivery',
    )
  }
  return Object.freeze({
    schemaVersion: STRONGFLOW_DELIVERY_API_SCHEMA_VERSION,
    requestId: request.requestId,
    operation: request.operation,
    ok: true,
    result: Object.freeze({ delivery, diagramExecution }),
  })
}

export function materializeStrongFlowDeliveryFailure(input: {
  readonly requestId: string | null
  readonly operation: StrongFlowDeliveryOperation | null
  readonly code: StrongFlowDeliveryErrorCode
  readonly message: string
  readonly currentRevision?: number | null
}): StrongFlowDeliveryFailureResponse {
  const requestIdValue = input.requestId === null
    ? null
    : portableRequestId(input.requestId, 'response.requestId', 'INVALID_RESPONSE')
  if (input.operation !== null && !STRONGFLOW_DELIVERY_OPERATIONS.includes(input.operation)) {
    apiError('INVALID_RESPONSE', 'response.operation', 'response operation is unsupported')
  }
  if (!STRONGFLOW_DELIVERY_ERROR_CODES.includes(input.code)) {
    apiError('INVALID_RESPONSE', 'response.error.code', 'response error code is unsupported')
  }
  const currentRevision = input.currentRevision ?? null
  if (currentRevision !== null
    && (!Number.isSafeInteger(currentRevision) || currentRevision < 1)) {
    apiError('INVALID_RESPONSE', 'response.error.currentRevision', 'currentRevision is invalid')
  }
  return Object.freeze({
    schemaVersion: STRONGFLOW_DELIVERY_API_SCHEMA_VERSION,
    requestId: requestIdValue,
    operation: input.operation,
    ok: false,
    error: Object.freeze({
      code: input.code,
      message: boundedText(input.message, 'response.error.message', 'INVALID_RESPONSE'),
      currentRevision,
    }),
  })
}

export function parseStrongFlowDeliveryResponse(value: unknown): StrongFlowDeliveryResponse {
  const input = record(value, 'response', 'INVALID_RESPONSE')
  if (input.schemaVersion !== STRONGFLOW_DELIVERY_API_SCHEMA_VERSION) {
    apiError(
      'UNSUPPORTED_SCHEMA_VERSION',
      'response.schemaVersion',
      'response schemaVersion is unsupported',
    )
  }
  if (input.ok === false) {
    exactKeys(
      input,
      ['schemaVersion', 'requestId', 'operation', 'ok', 'error'],
      'response',
      'INVALID_RESPONSE',
    )
    const error = record(input.error, 'response.error', 'INVALID_RESPONSE')
    exactKeys(error, ['code', 'message', 'currentRevision'], 'response.error', 'INVALID_RESPONSE')
    return materializeStrongFlowDeliveryFailure({
      requestId: input.requestId === null
        ? null
        : portableRequestId(input.requestId, 'response.requestId', 'INVALID_RESPONSE'),
      operation: input.operation === null
        ? null
        : operation(input.operation, 'response.operation', 'INVALID_RESPONSE'),
      code: responseErrorCode(error.code),
      message: boundedText(error.message, 'response.error.message', 'INVALID_RESPONSE'),
      currentRevision: error.currentRevision === null
        ? null
        : revision(error.currentRevision, 'response.error.currentRevision', 'INVALID_RESPONSE'),
    })
  }
  exactKeys(
    input,
    ['schemaVersion', 'requestId', 'operation', 'ok', 'result'],
    'response',
    'INVALID_RESPONSE',
  )
  if (input.ok !== true) apiError('INVALID_RESPONSE', 'response.ok', 'response ok is invalid')
  const selectedOperation = operation(input.operation, 'response.operation', 'INVALID_RESPONSE')
  const result = record(input.result, 'response.result', 'INVALID_RESPONSE')
  exactKeys(result, ['delivery', 'diagramExecution'], 'response.result', 'INVALID_RESPONSE')
  const delivery = parseResponseDelivery(result.delivery)
  const diagramExecution = result.diagramExecution === null
    ? null
    : parseResponseDiagramExecution(result.diagramExecution)
  if (diagramExecution !== null
    && (diagramExecution.deliveryId !== delivery.id
      || diagramExecution.deliveryRevision !== delivery.revision)) {
    apiError(
      'RELATIONSHIP_MISMATCH',
      'response.result.diagramExecution',
      'response diagram execution projection does not match the Delivery',
    )
  }
  return Object.freeze({
    schemaVersion: STRONGFLOW_DELIVERY_API_SCHEMA_VERSION,
    requestId: portableRequestId(input.requestId, 'response.requestId', 'INVALID_RESPONSE'),
    operation: selectedOperation,
    ok: true,
    result: Object.freeze({
      delivery,
      diagramExecution,
    }),
  }) as StrongFlowDeliverySuccessResponse
}

export function parseStrongFlowDeliveryResponseForRequest<
  Operation extends StrongFlowDeliveryOperation,
>(
  request: StrongFlowDeliveryRequestFor<Operation>,
  responseValue: unknown,
): StrongFlowDeliveryResponseFor<Operation> {
  const response = parseStrongFlowDeliveryResponse(responseValue)
  if (response.requestId !== request.requestId || response.operation !== request.operation) {
    apiError(
      'RELATIONSHIP_MISMATCH',
      'response',
      'response identity does not match the request',
    )
  }
  if (response.ok
    && response.result.delivery.id !== requestDeliveryId(request as StrongFlowDeliveryRequest)) {
    apiError(
      'RELATIONSHIP_MISMATCH',
      'response.result.delivery.id',
      'response delivery does not match the request',
    )
  }
  const authenticationProof = request.operation === 'resolveAttention'
    ? (request as StrongFlowDeliveryRequestFor<'resolveAttention'>)
      .payload.authentication.proof
    : null
  if (authenticationProof !== null && containsText(response, authenticationProof)) {
    apiError(
      'RELATIONSHIP_MISMATCH',
      'response',
      'response contains authentication material from the request',
    )
  }
  return response as StrongFlowDeliveryResponseFor<Operation>
}

export interface StrongFlowDeliveryInvoker {
  invoke<Operation extends StrongFlowDeliveryOperation>(
    request: StrongFlowDeliveryRequestFor<Operation>,
    options?: { readonly signal?: AbortSignal },
  ): Promise<StrongFlowDeliveryResponseFor<Operation>>
}
