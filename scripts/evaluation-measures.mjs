import {
  DELIVERY_MEASURES_SCHEMA_VERSION,
  createDeliveryMeasuresProjection,
} from '../packages/strongflow/dist/index.js'

export const LIVE_EVALUATION_VERIFICATION_ROLES = Object.freeze([
  'reviewer',
  'verifier',
])

function isRecord(value) {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

function requiredRecord(value, path) {
  if (!isRecord(value)) throw new TypeError(`${path} must be an object`)
  return value
}

function compatibleRuntimeProjection(result) {
  if (result.delivery === null || result.runtimeProjection === null) return null
  const projection = requiredRecord(result.runtimeProjection, 'result.runtimeProjection')
  return projection.deliveryId === result.delivery.id
    && projection.deliveryRevision === result.delivery.revision
    ? projection
    : null
}

function modelCallFact(runId, call, index) {
  const usage = call.usage === null ? null : requiredRecord(
    call.usage,
    `result.budget.calls[${String(index)}].usage`,
  )
  return Object.freeze({
    sourceRef: `evaluation_run:${runId}#/budget/calls/${String(index)}`,
    status: call.status,
    startedAtMillis: call.startedAtMillis,
    finishedAtMillis: call.finishedAtMillis,
    inputTokens: usage?.inputTokens ?? null,
    outputTokens: usage?.outputTokens ?? null,
    cacheReadTokens: usage?.cacheReadTokens ?? null,
    cacheWriteTokens: usage?.cacheWriteTokens ?? null,
    costUsdMicros: call.costUsdMicros ?? null,
  })
}

/** Recompute one source-linked projection from a stored live result. */
export function measureLiveEvaluationResult(value) {
  const result = requiredRecord(value, 'result')
  const budget = requiredRecord(result.budget, 'result.budget')
  const limits = requiredRecord(budget.limits, 'result.budget.limits')
  const pricing = requiredRecord(limits.pricing, 'result.budget.limits.pricing')
  if (!Array.isArray(budget.calls)) {
    throw new TypeError('result.budget.calls must be an array')
  }
  const delivery = result.delivery === null
    ? null
    : requiredRecord(result.delivery, 'result.delivery')
  const historicalVerdicts = delivery?.verdict === null || delivery === null
    ? []
    : [delivery.verdict]
  return createDeliveryMeasuresProjection({
    schemaVersion: DELIVERY_MEASURES_SCHEMA_VERSION,
    runKind: 'live',
    runId: result.runId,
    runState: result.state,
    startedAtMillis: result.startedAtMillis,
    finishedAtMillis: result.finishedAtMillis,
    delivery,
    runtimeProjection: compatibleRuntimeProjection(result),
    requiredVerificationRoles: LIVE_EVALUATION_VERIFICATION_ROLES,
    modelCalls: budget.calls.map((call, index) => modelCallFact(
      result.runId,
      requiredRecord(call, `result.budget.calls[${String(index)}]`),
      index,
    )),
    pricingSource: pricing.source,
    historicalVerdicts,
  })
}
