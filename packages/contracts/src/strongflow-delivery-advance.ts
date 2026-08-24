import {
  DeliveryId,
  StageRunId,
  parseDelivery,
  type Delivery,
  type DeliveryId as DeliveryIdentifier,
  type StageRunId as StageRunIdentifier,
} from './delivery.js'

export const STRONGFLOW_DELIVERY_ADVANCE_SCHEMA_VERSION = 1 as const

export const STRONGFLOW_DELIVERY_ADVANCE_OUTCOMES = Object.freeze([
  'plan-review-ready',
  'candidate-ready-for-review',
  'reviewer-complete',
  'delivery-review-ready',
  'attention-required',
  'stage-busy',
  'delivery-complete',
] as const)

export type StrongFlowDeliveryAdvanceOutcomeKind =
  typeof STRONGFLOW_DELIVERY_ADVANCE_OUTCOMES[number]

export const STRONGFLOW_DELIVERY_ADVANCE_ERROR_CODES = Object.freeze([
  'INVALID_REQUEST',
  'DELIVERY_NOT_FOUND',
  'REVISION_CONFLICT',
  'DELIVERY_STATE_CONFLICT',
  'MODEL_SELECTION_REQUIRED',
  'UNSUPPORTED_REPOSITORY',
  'STAGE_BUSY',
  'RUNTIME_INTERACTION_REQUIRED',
  'STAGE_OUTPUT_INVALID',
  'CANDIDATE_FAILURE',
  'OPERATION_ABORTED',
  'INTERNAL_ERROR',
] as const)

export type StrongFlowDeliveryAdvanceErrorCode =
  typeof STRONGFLOW_DELIVERY_ADVANCE_ERROR_CODES[number]

export interface StrongFlowDeliveryAdvanceRequest {
  readonly schemaVersion: typeof STRONGFLOW_DELIVERY_ADVANCE_SCHEMA_VERSION
  readonly requestId: string
  readonly deliveryId: DeliveryIdentifier
  readonly expectedRevision: number
}

export interface StrongFlowDeliveryAdvanceOutcome {
  readonly kind: StrongFlowDeliveryAdvanceOutcomeKind
  readonly message: string
  readonly stageRunId: StageRunIdentifier | null
  readonly dshSessionId: string | null
}

export type StrongFlowDeliveryAdvanceResponse =
  | {
      readonly schemaVersion: typeof STRONGFLOW_DELIVERY_ADVANCE_SCHEMA_VERSION
      readonly requestId: string
      readonly ok: true
      readonly result: {
        readonly delivery: Delivery
        readonly outcome: StrongFlowDeliveryAdvanceOutcome
      }
    }
  | {
      readonly schemaVersion: typeof STRONGFLOW_DELIVERY_ADVANCE_SCHEMA_VERSION
      readonly requestId: string | null
      readonly ok: false
      readonly error: {
        readonly code: StrongFlowDeliveryAdvanceErrorCode
        readonly message: string
        readonly currentRevision: number | null
      }
    }

const REQUEST_ID_PATTERN = /^[A-Za-z0-9][A-Za-z0-9._:/@-]{0,499}$/u
const SESSION_ID_PATTERN = /^[A-Za-z0-9][A-Za-z0-9._:/@-]{0,499}$/u
const MAX_MESSAGE_LENGTH = 65_536

function isRecord(value: unknown): value is Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return false
  const prototype = Object.getPrototypeOf(value)
  return prototype === Object.prototype || prototype === null
}

function record(value: unknown, label: string): Record<string, unknown> {
  if (!isRecord(value)) throw new TypeError(`${label} must be an object`)
  return value
}

function exactKeys(value: Record<string, unknown>, keys: readonly string[], label: string): void {
  const expected = new Set(keys)
  if (Object.keys(value).length !== expected.size
    || keys.some(key => !Object.hasOwn(value, key))
    || Object.keys(value).some(key => !expected.has(key))) {
    throw new TypeError(`${label} has an unexpected shape`)
  }
}

function requestId(value: unknown, nullable = false): string | null {
  if (nullable && value === null) return null
  if (typeof value !== 'string' || !REQUEST_ID_PATTERN.test(value)) {
    throw new TypeError('requestId is invalid')
  }
  return value
}

function revision(value: unknown): number {
  if (!Number.isSafeInteger(value) || Number(value) < 1) {
    throw new TypeError('expectedRevision must be a positive safe integer')
  }
  return Number(value)
}

function message(value: unknown): string {
  if (typeof value !== 'string'
    || value.trim().length === 0
    || value.length > MAX_MESSAGE_LENGTH) {
    throw new TypeError('message must be bounded non-empty text')
  }
  return value
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

export function parseStrongFlowDeliveryAdvanceRequest(
  value: unknown,
): StrongFlowDeliveryAdvanceRequest {
  const input = record(value, 'advance request')
  exactKeys(input, ['schemaVersion', 'requestId', 'deliveryId', 'expectedRevision'], 'advance request')
  if (input.schemaVersion !== STRONGFLOW_DELIVERY_ADVANCE_SCHEMA_VERSION) {
    throw new TypeError('advance request schemaVersion is unsupported')
  }
  if (typeof input.deliveryId !== 'string') throw new TypeError('deliveryId is invalid')
  return Object.freeze({
    schemaVersion: STRONGFLOW_DELIVERY_ADVANCE_SCHEMA_VERSION,
    requestId: requestId(input.requestId)!,
    deliveryId: DeliveryId(input.deliveryId),
    expectedRevision: revision(input.expectedRevision),
  })
}

export function materializeStrongFlowDeliveryAdvanceRequest(
  requestIdValue: string,
  deliveryIdValue: DeliveryIdentifier | string,
  expectedRevision: number,
): StrongFlowDeliveryAdvanceRequest {
  return parseStrongFlowDeliveryAdvanceRequest({
    schemaVersion: STRONGFLOW_DELIVERY_ADVANCE_SCHEMA_VERSION,
    requestId: requestIdValue,
    deliveryId: deliveryIdValue,
    expectedRevision,
  })
}

function parseOutcome(value: unknown): StrongFlowDeliveryAdvanceOutcome {
  const input = record(value, 'advance response.result.outcome')
  exactKeys(input, ['kind', 'message', 'stageRunId', 'dshSessionId'], 'advance response.result.outcome')
  if (typeof input.kind !== 'string'
    || !STRONGFLOW_DELIVERY_ADVANCE_OUTCOMES.includes(
      input.kind as StrongFlowDeliveryAdvanceOutcomeKind,
    )) throw new TypeError('advance outcome kind is unsupported')
  if (input.dshSessionId !== null
    && (typeof input.dshSessionId !== 'string'
      || !SESSION_ID_PATTERN.test(input.dshSessionId))) {
    throw new TypeError('advance outcome DSH Session id is invalid')
  }
  return Object.freeze({
    kind: input.kind as StrongFlowDeliveryAdvanceOutcomeKind,
    message: message(input.message),
    stageRunId: input.stageRunId === null
      ? null
      : StageRunId(typeof input.stageRunId === 'string' ? input.stageRunId : ''),
    dshSessionId: input.dshSessionId as string | null,
  })
}

export function materializeStrongFlowDeliveryAdvanceSuccess(
  request: StrongFlowDeliveryAdvanceRequest,
  delivery: Delivery,
  outcome: StrongFlowDeliveryAdvanceOutcome,
): StrongFlowDeliveryAdvanceResponse {
  return parseStrongFlowDeliveryAdvanceResponse({
    schemaVersion: STRONGFLOW_DELIVERY_ADVANCE_SCHEMA_VERSION,
    requestId: request.requestId,
    ok: true,
    result: { delivery, outcome },
  })
}

export function materializeStrongFlowDeliveryAdvanceFailure(input: {
  readonly requestId: string | null
  readonly code: StrongFlowDeliveryAdvanceErrorCode
  readonly message: string
  readonly currentRevision?: number | null
}): StrongFlowDeliveryAdvanceResponse {
  return parseStrongFlowDeliveryAdvanceResponse({
    schemaVersion: STRONGFLOW_DELIVERY_ADVANCE_SCHEMA_VERSION,
    requestId: input.requestId,
    ok: false,
    error: {
      code: input.code,
      message: input.message,
      currentRevision: input.currentRevision ?? null,
    },
  })
}

export function parseStrongFlowDeliveryAdvanceResponse(
  value: unknown,
): StrongFlowDeliveryAdvanceResponse {
  const input = record(value, 'advance response')
  if (input.ok === true) {
    exactKeys(input, ['schemaVersion', 'requestId', 'ok', 'result'], 'advance response')
    if (input.schemaVersion !== STRONGFLOW_DELIVERY_ADVANCE_SCHEMA_VERSION) {
      throw new TypeError('advance response schemaVersion is unsupported')
    }
    const result = record(input.result, 'advance response.result')
    exactKeys(result, ['delivery', 'outcome'], 'advance response.result')
    return immutable({
      schemaVersion: STRONGFLOW_DELIVERY_ADVANCE_SCHEMA_VERSION,
      requestId: requestId(input.requestId)!,
      ok: true,
      result: {
        delivery: parseDelivery(result.delivery, 'advance response.result.delivery'),
        outcome: parseOutcome(result.outcome),
      },
    })
  }
  if (input.ok !== false) throw new TypeError('advance response ok is invalid')
  exactKeys(input, ['schemaVersion', 'requestId', 'ok', 'error'], 'advance response')
  if (input.schemaVersion !== STRONGFLOW_DELIVERY_ADVANCE_SCHEMA_VERSION) {
    throw new TypeError('advance response schemaVersion is unsupported')
  }
  const error = record(input.error, 'advance response.error')
  exactKeys(error, ['code', 'message', 'currentRevision'], 'advance response.error')
  if (typeof error.code !== 'string'
    || !STRONGFLOW_DELIVERY_ADVANCE_ERROR_CODES.includes(
      error.code as StrongFlowDeliveryAdvanceErrorCode,
    )) throw new TypeError('advance response error code is unsupported')
  if (error.currentRevision !== null) revision(error.currentRevision)
  return immutable({
    schemaVersion: STRONGFLOW_DELIVERY_ADVANCE_SCHEMA_VERSION,
    requestId: requestId(input.requestId, true),
    ok: false,
    error: {
      code: error.code as StrongFlowDeliveryAdvanceErrorCode,
      message: message(error.message),
      currentRevision: error.currentRevision as number | null,
    },
  })
}
