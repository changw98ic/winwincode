import assert from 'node:assert/strict'
import test from 'node:test'

import {
  DELIVERY_SCHEMA_VERSION,
  parseDelivery,
} from '../packages/contracts/dist/index.js'
import {
  AcceptanceVerificationError,
  assertAcceptanceVerificationInputCurrent,
  freezeAcceptanceVerificationInput,
} from '../packages/strongflow/dist/index.js'

const now = 2_400_000_000_000
const deliveryId = 'dlv_7XRHEBX4M4R28PNKHG00H19BC9'

function criterion(id, { required, verificationMethod }) {
  return {
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id,
    description: `Observe ${id} on the exact delivery candidate.`,
    verificationMethod,
    required,
  }
}

function approvedDelivery({
  specRevision = 1,
  specSuffix = `v${specRevision}`,
  withApprovalSession = true,
  secondApproval = false,
} = {}) {
  const specId = `delivery-spec-${specSuffix}`
  const planRun = id => ({
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: `stage-plan-review-${id}`,
    deliveryId,
    deliveryTaskId: null,
    stage: 'plan-review',
    actorType: 'human',
    role: 'reviewer',
    status: 'succeeded',
    attempt: 1,
    startedAtMillis: now + 10,
    finishedAtMillis: now + 20,
  })
  const approval = id => ({
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: `attention-plan-review-${id}`,
    deliveryId,
    deliverySpecId: specId,
    stageRunId: `stage-plan-review-${id}`,
    type: 'decision_required',
    title: 'Approve the plan and current acceptance definition',
    context: `Review the solution against ${specId}.`,
    options: [],
    assignedTo: 'reviewer-1',
    blocking: true,
    status: 'resolved',
    resolution: `Approved ${specId}.`,
    resolvedBy: 'reviewer-1',
    createdAtMillis: now + 11,
    resolvedAtMillis: now + 19,
  })
  const ids = secondApproval ? ['one', 'two'] : ['one']
  return parseDelivery({
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: deliveryId,
    revision: specRevision + 5,
    status: 'executing',
    spec: {
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: specId,
      deliveryId,
      revision: specRevision,
      title: `Acceptance freeze ${specSuffix}`,
      goal: 'Verify the approved criteria without weakening them during execution.',
      scope: ['Acceptance verification'],
      outOfScope: ['A second test runner'],
      constraints: ['Codex remains the execution authority'],
      acceptanceCriteria: [
        criterion(`criterion-required-${specSuffix}`, {
          required: true,
          verificationMethod: 'Run pnpm test and retain the exact runtime event.',
        }),
        criterion(`criterion-review-${specSuffix}`, {
          required: false,
          verificationMethod: null,
        }),
      ],
      sourceRef: null,
      publicationTarget: null,
      repository: {
        schemaVersion: DELIVERY_SCHEMA_VERSION,
        kind: 'local-git',
        locator: '/workspace/repository',
      },
      baseRevision: '0123456789012345678901234567890123456789',
      maxReworkAttempts: 2,
      createdAtMillis: now + specRevision,
    },
    tasks: [],
    stageRuns: ids.map(planRun),
    sessionBindings: withApprovalSession
      ? ids.map(id => ({
          schemaVersion: DELIVERY_SCHEMA_VERSION,
          id: `binding-plan-review-${id}`,
          deliveryId,
          stageRunId: `stage-plan-review-${id}`,
          dshSessionId: `dsh-plan-review-${id}`,
          codexSessionId: null,
          boundAtMillis: now + 12,
        }))
      : [],
    attentionItems: ids.map(approval),
    evidence: [],
    verdict: null,
    createdAtMillis: now,
    updatedAtMillis: now + 20,
  })
}

function expectAcceptanceError(code) {
  return error => error instanceof AcceptanceVerificationError && error.code === code
}

test('freezes the exact approved criteria and keeps missing checks explicit', () => {
  const delivery = approvedDelivery()
  const input = freezeAcceptanceVerificationInput(delivery)

  assert.equal(input.deliveryId, delivery.id)
  assert.equal(input.deliverySpecId, delivery.spec.id)
  assert.equal(input.deliverySpecRevision, delivery.spec.revision)
  assert.match(input.freezeId, /^sha256:[a-f0-9]{64}$/u)
  assert.equal(input.approval.attentionItemId, 'attention-plan-review-one')
  assert.equal(input.approval.stageRunId, 'stage-plan-review-one')
  assert.equal(input.approval.approvedBy, 'reviewer-1')
  assert.deepEqual(input.approval.sessionBindingIds, ['binding-plan-review-one'])
  assert.deepEqual(input.criteria.map(entry => entry.criterion), delivery.spec.acceptanceCriteria)
  assert.deepEqual(input.criteria[0].evidenceRequirement, {
    kind: 'declared-check',
    verificationMethod: 'Run pnpm test and retain the exact runtime event.',
  })
  assert.deepEqual(input.criteria[1].evidenceRequirement, {
    kind: 'attention-required',
    attentionType: 'verification_blocked',
    reason: 'verification_method_missing',
  })
  assert.equal(Object.isFrozen(input), true)
  assert.equal(Object.isFrozen(input.criteria), true)
  assert.equal(Object.isFrozen(input.criteria[0].criterion), true)
  assert.deepEqual(assertAcceptanceVerificationInputCurrent(delivery, input), input)
})

test('rejects mutation and a later DeliverySpec revision as stale verification input', () => {
  const firstDelivery = approvedDelivery()
  const input = freezeAcceptanceVerificationInput(firstDelivery)
  const weakened = structuredClone(input)
  weakened.criteria[0].criterion.required = false
  assert.throws(
    () => assertAcceptanceVerificationInputCurrent(firstDelivery, weakened),
    expectAcceptanceError('ACCEPTANCE_INPUT_STALE'),
  )

  const reduced = structuredClone(input)
  reduced.criteria.splice(0, 1)
  assert.throws(
    () => assertAcceptanceVerificationInputCurrent(firstDelivery, reduced),
    expectAcceptanceError('ACCEPTANCE_INPUT_STALE'),
  )

  const revisedDelivery = approvedDelivery({ specRevision: 2, specSuffix: 'v2' })
  assert.throws(
    () => assertAcceptanceVerificationInputCurrent(revisedDelivery, input),
    expectAcceptanceError('ACCEPTANCE_INPUT_STALE'),
  )
  assert.notEqual(freezeAcceptanceVerificationInput(revisedDelivery).freezeId, input.freezeId)
})

test('requires one approved human review with a bound DSH session', () => {
  const unapproved = parseDelivery({
    ...approvedDelivery(),
    revision: 7,
    status: 'planning',
    stageRuns: [],
    sessionBindings: [],
    attentionItems: [],
    updatedAtMillis: now + 21,
  })
  assert.throws(
    () => freezeAcceptanceVerificationInput(unapproved),
    expectAcceptanceError('ACCEPTANCE_NOT_APPROVED'),
  )
  assert.throws(
    () => freezeAcceptanceVerificationInput(approvedDelivery({ withApprovalSession: false })),
    expectAcceptanceError('APPROVAL_SESSION_UNBOUND'),
  )
  assert.throws(
    () => freezeAcceptanceVerificationInput(approvedDelivery({ secondApproval: true })),
    expectAcceptanceError('APPROVAL_AMBIGUOUS'),
  )
})
