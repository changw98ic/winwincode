import assert from 'node:assert/strict'
import test from 'node:test'

import {
  DELIVERY_SCHEMA_VERSION,
  STRONGFLOW_DELIVERY_API_SCHEMA_VERSION,
  STRONGFLOW_DIAGRAM_EXECUTION_PROTOCOL,
  STRONGFLOW_DIAGRAM_EXECUTION_SCHEMA_VERSION,
  StrongFlowDeliveryApiValidationError,
  materializeStrongFlowDeliveryFailure,
  materializeStrongFlowDeliveryRequest,
  materializeStrongFlowDeliverySuccess,
  parseStrongFlowDeliveryRequest,
  parseStrongFlowDeliveryResponse,
  parseStrongFlowDeliveryResponseForRequest,
} from '../packages/contracts/dist/index.js'

const now = 1_800_000_000_000
const deliveryId = 'delivery-api-fixture'
const proof = 'fixture-local-proof-value'

function spec() {
  return {
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: 'delivery-spec-api-v1',
    deliveryId,
    revision: 1,
    title: 'Delivery API fixture',
    goal: 'Exercise the shared Delivery transport boundary.',
    scope: ['Delivery transport'],
    outOfScope: ['Generic task management'],
    constraints: ['Codex remains the execution authority'],
    acceptanceCriteria: [{
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      id: 'criterion-api-required',
      description: 'The transport round-trips every supported operation.',
      verificationMethod: 'Run this direct contract test.',
      required: true,
    }],
    sourceRef: null,
    publicationTarget: null,
    repository: {
      schemaVersion: DELIVERY_SCHEMA_VERSION,
      kind: 'local-git',
      locator: '/workspace/repository',
    },
    baseRevision: '0123456789012345678901234567890123456789',
    maxReworkAttempts: 2,
    createdAtMillis: now,
  }
}

function task() {
  return {
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: 'delivery-task-api',
    deliveryId,
    title: 'Shared API',
    goal: 'Provide one strict host and UI call path.',
    acceptanceCriterionIds: ['criterion-api-required'],
    blockedByTaskIds: [],
    owner: null,
    status: 'pending',
  }
}

function draftDelivery() {
  return {
    schemaVersion: DELIVERY_SCHEMA_VERSION,
    id: deliveryId,
    revision: 1,
    status: 'draft',
    spec: spec(),
    tasks: [task()],
    stageRuns: [],
    sessionBindings: [],
    attentionItems: [],
    evidence: [],
    verdict: null,
    createdAtMillis: now,
    updatedAtMillis: now,
  }
}

function candidate() {
  return {
    schemaVersion: 1,
    candidateRef: `git-candidate:sha256:${'a'.repeat(64)}`,
    deliveryId,
    deliverySpecId: spec().id,
    deliverySpecRevision: spec().revision,
    repositoryKind: 'local-git',
    repositoryLocator: '/workspace/repository',
    baseRevision: spec().baseRevision,
    producerStageRunId: 'stage-api-executor',
    producerSessionBindingId: 'binding-api-executor',
    baseCommitId: spec().baseRevision,
    baseTreeId: 'b'.repeat(40),
    candidateCommitId: 'c'.repeat(40),
    candidateTreeId: 'd'.repeat(40),
    diffSha256: 'e'.repeat(64),
    changedPaths: [{
      path: 'src/result.ts',
      state: 'present',
      objectId: 'f'.repeat(40),
    }],
  }
}

function liveDiagramExecution() {
  return {
    schemaVersion: STRONGFLOW_DIAGRAM_EXECUTION_SCHEMA_VERSION,
    protocol: STRONGFLOW_DIAGRAM_EXECUTION_PROTOCOL,
    deliveryId,
    deliveryRevision: 1,
    reviewSetSha256: '2'.repeat(64),
    state: 'executing',
    architecture: {
      diagramId: 'diagram-api-architecture',
      kind: 'system-architecture',
      nodes: [{
        nodeId: 'node-api-component',
        state: 'affected-live',
        affectedFileCount: 1,
        fileIds: [],
      }],
    },
    process: {
      diagramId: 'diagram-api-process',
      kind: 'process-flow',
      nodes: [{
        nodeId: 'node-api-executing',
        state: 'affected-live',
        affectedFileCount: 1,
        fileIds: [],
      }],
    },
    affectedFileCount: 1,
    details: null,
    updatedAtMillis: now,
  }
}

function remediation() {
  return {
    schemaVersion: STRONGFLOW_DELIVERY_API_SCHEMA_VERSION,
    protocol: 'winwincode.delivery-remediation.v1',
    deliveryTaskId: 'delivery-task-api',
    candidate: candidate(),
    annotations: [{
      schemaVersion: STRONGFLOW_DELIVERY_API_SCHEMA_VERSION,
      id: 'annotation-api-result',
      diagramKind: 'process-flow',
      diagramId: 'diagram-api-flow',
      nodeId: 'node-api-result',
      filePath: 'src/result.ts',
      hunkSha256: '1'.repeat(64),
      evidenceRefIds: ['evidence-api-result'],
      note: 'Correct the selected result path without changing the approved scope.',
    }],
  }
}

function runtimeEvents() {
  return [{
    schemaVersion: 1,
    id: 'dsh-api-verifier@1',
    cursor: { sessionId: 'dsh-api-verifier', sequence: '1' },
    kind: 'session.configured',
    source: {
      authority: 'codex-core',
      sessionId: 'dsh-api-verifier',
      kernelSessionId: 'codex-api-verifier',
      roleId: 'verifier',
      kernelStreamId: 'stream-api-verifier',
      kernelSequence: '1',
      submissionId: 'submission-api-verifier',
      kernelKind: 'session_configured',
    },
    data: {},
  }]
}

function expectApiError(code, path, action) {
  assert.throws(action, error => (
    error instanceof StrongFlowDeliveryApiValidationError
      && error.code === code
      && error.path === path
  ))
}

test('Delivery API round-trips every canonical service operation', () => {
  const requests = [
    materializeStrongFlowDeliveryRequest('createDelivery', 'request-create', {
      spec: spec(),
      tasks: [task()],
    }),
    materializeStrongFlowDeliveryRequest('updateDeliverySpec', 'request-update', {
      deliveryId,
      expectedRevision: 1,
      spec: { ...spec(), id: 'delivery-spec-api-v2', revision: 2 },
    }),
    materializeStrongFlowDeliveryRequest('startStage', 'request-stage', {
      deliveryId,
      expectedRevision: 2,
      stageRunId: 'stage-api-planning',
      deliveryTaskId: null,
      stage: 'planning',
      actorType: 'codex',
      role: 'planner',
      attention: null,
    }),
    materializeStrongFlowDeliveryRequest('bindSession', 'request-bind', {
      deliveryId,
      expectedRevision: 3,
      bindingId: 'binding-api-planner',
      stageRunId: 'stage-api-planning',
      dshSessionId: 'dsh-session-api',
      codexSessionId: 'codex-session-api',
    }),
    materializeStrongFlowDeliveryRequest('resolveAttention', 'request-attention', {
      deliveryId,
      expectedRevision: 4,
      attentionItemId: 'attention-api-decision',
      status: 'resolved',
      resolution: 'Approve the reviewed plan.',
      remediation: null,
      channel: 'local-ui',
      authentication: { scheme: 'local-session', proof },
    }),
    materializeStrongFlowDeliveryRequest('resolveAttention', 'request-remediation', {
      deliveryId,
      expectedRevision: 5,
      attentionItemId: 'attention-api-delivery-review',
      status: 'dismissed',
      resolution: 'Apply the selected diagram annotation.',
      remediation: remediation(),
      channel: 'local-ui',
      authentication: { scheme: 'local-session', proof },
    }),
    materializeStrongFlowDeliveryRequest('submitVerdict', 'request-verdict', {
      deliveryId,
      expectedRevision: 5,
      candidate: candidate(),
      runtimeEvents: runtimeEvents(),
      requiredRoles: ['reviewer', 'verifier'],
    }),
    materializeStrongFlowDeliveryRequest('getDeliveryProjection', 'request-show', {
      deliveryId,
    }),
  ]

  for (const request of requests) {
    assert.deepEqual(
      parseStrongFlowDeliveryRequest(JSON.parse(JSON.stringify(request))),
      request,
    )
    assert.ok(Object.isFrozen(request))
    assert.ok(Object.isFrozen(request.payload))
  }
})

test('Delivery API rejects extra fields, empty bindings, and mismatched authentication', () => {
  expectApiError('UNSUPPORTED_SCHEMA_VERSION', 'request.schemaVersion', () => (
    parseStrongFlowDeliveryRequest({
      schemaVersion: STRONGFLOW_DELIVERY_API_SCHEMA_VERSION - 1,
      requestId: 'request-old-api-version',
      operation: 'getDeliveryProjection',
      payload: { deliveryId },
    })
  ))
  expectApiError('INVALID_REQUEST', 'request', () => parseStrongFlowDeliveryRequest({
    schemaVersion: STRONGFLOW_DELIVERY_API_SCHEMA_VERSION,
    requestId: 'request-extra',
    operation: 'getDeliveryProjection',
    payload: { deliveryId },
    codexPlan: [],
  }))
  expectApiError('INVALID_REQUEST', 'request.payload', () => (
    materializeStrongFlowDeliveryRequest('bindSession', 'request-empty-binding', {
      deliveryId,
      expectedRevision: 1,
      bindingId: 'binding-empty',
      stageRunId: 'stage-empty',
      dshSessionId: null,
      codexSessionId: null,
    })
  ))
  expectApiError(
    'INVALID_REQUEST',
    'request.payload.authentication.scheme',
    () => materializeStrongFlowDeliveryRequest('resolveAttention', 'request-wrong-auth', {
      deliveryId,
      expectedRevision: 1,
      attentionItemId: 'attention-api-decision',
      status: 'resolved',
      resolution: 'Approve the reviewed plan.',
      remediation: null,
      channel: 'local-ui',
      authentication: { scheme: 'local-peer', proof },
    }),
  )
  expectApiError('INVALID_REQUEST', 'request.payload.requiredRoles', () => (
    materializeStrongFlowDeliveryRequest('submitVerdict', 'request-missing-reviewer', {
      deliveryId,
      expectedRevision: 1,
      candidate: candidate(),
      runtimeEvents: runtimeEvents(),
      requiredRoles: ['verifier'],
    })
  ))
  expectApiError('INVALID_REQUEST', 'request.payload', () => (
    materializeStrongFlowDeliveryRequest('submitVerdict', 'request-caller-verdict', {
      deliveryId,
      expectedRevision: 1,
      evidence: [],
      verdict: { status: 'pass' },
    })
  ))
  expectApiError('INVALID_REQUEST', 'request.payload', () => (
    materializeStrongFlowDeliveryRequest('submitVerdict', 'request-caller-attention', {
      deliveryId,
      expectedRevision: 1,
      candidate: candidate(),
      runtimeEvents: runtimeEvents(),
      requiredRoles: ['reviewer', 'verifier'],
      attention: null,
    })
  ))
  expectApiError('INVALID_REQUEST', 'request.payload', () => (
    materializeStrongFlowDeliveryRequest('resolveAttention', 'request-caller-next-state', {
      deliveryId,
      expectedRevision: 1,
      attentionItemId: 'attention-api-decision',
      status: 'resolved',
      resolution: 'Approve the reviewed plan.',
      remediation: null,
      channel: 'local-ui',
      authentication: { scheme: 'local-session', proof },
      nextStatus: 'executing',
    })
  ))
})

test('Delivery API accepts only bounded candidate-bound diagram remediation', () => {
  const payload = {
    deliveryId,
    expectedRevision: 1,
    attentionItemId: 'attention-api-delivery-review',
    status: 'dismissed',
    resolution: 'Apply the selected diagram annotation.',
    remediation: remediation(),
    channel: 'local-ui',
    authentication: { scheme: 'local-session', proof },
  }
  const parsed = materializeStrongFlowDeliveryRequest(
    'resolveAttention',
    'request-valid-remediation',
    payload,
  )
  assert.equal(parsed.payload.remediation.candidate.diffSha256, 'e'.repeat(64))
  assert.equal(parsed.payload.remediation.annotations[0].filePath, 'src/result.ts')

  expectApiError('INVALID_REQUEST', 'request.payload', () => (
    parseStrongFlowDeliveryRequest({
      schemaVersion: STRONGFLOW_DELIVERY_API_SCHEMA_VERSION,
      requestId: 'request-old-resolution-shape',
      operation: 'resolveAttention',
      payload: {
        deliveryId,
        expectedRevision: 1,
        attentionItemId: 'attention-api-delivery-review',
        status: 'dismissed',
        resolution: 'Old caller omitted structured remediation.',
        channel: 'local-ui',
        authentication: { scheme: 'local-session', proof },
      },
    })
  ))
  expectApiError(
    'INVALID_REQUEST',
    'request.payload.remediation.annotations[0].filePath',
    () => materializeStrongFlowDeliveryRequest(
      'resolveAttention',
      'request-traversing-remediation',
      {
        ...payload,
        remediation: {
          ...remediation(),
          annotations: [{
            ...remediation().annotations[0],
            filePath: '../outside.ts',
          }],
        },
      },
    ),
  )
  expectApiError(
    'INVALID_REQUEST',
    'request.payload.remediation.annotations[0].evidenceRefIds',
    () => materializeStrongFlowDeliveryRequest(
      'resolveAttention',
      'request-unbound-remediation',
      {
        ...payload,
        remediation: {
          ...remediation(),
          annotations: [{
            ...remediation().annotations[0],
            evidenceRefIds: [],
          }],
        },
      },
    ),
  )
})

test('Delivery API binds a success response to the exact request and Delivery', () => {
  const request = materializeStrongFlowDeliveryRequest(
    'getDeliveryProjection',
    'request-response',
    { deliveryId },
  )
  const response = materializeStrongFlowDeliverySuccess(request, draftDelivery())
  assert.deepEqual(
    parseStrongFlowDeliveryResponseForRequest(request, JSON.parse(JSON.stringify(response))),
    response,
  )
  expectApiError('RELATIONSHIP_MISMATCH', 'response', () => (
    parseStrongFlowDeliveryResponseForRequest(
      { ...request, requestId: 'request-other' },
      response,
    )
  ))
  expectApiError('RELATIONSHIP_MISMATCH', 'response.result.delivery.id', () => (
    materializeStrongFlowDeliverySuccess(request, {
      ...draftDelivery(),
      id: 'delivery-other',
      spec: { ...spec(), deliveryId: 'delivery-other' },
      tasks: [{ ...task(), deliveryId: 'delivery-other' }],
    })
  ))
})

test('Delivery API keeps live diagram details absent and rejects stale or exposed projections', () => {
  const request = materializeStrongFlowDeliveryRequest(
    'getDeliveryProjection',
    'request-live-diagram-response',
    { deliveryId },
  )
  const response = materializeStrongFlowDeliverySuccess(
    request,
    draftDelivery(),
    liveDiagramExecution(),
  )
  const serialized = JSON.stringify(response)
  assert.equal(response.result.diagramExecution.state, 'executing')
  assert.equal(response.result.diagramExecution.details, null)
  assert.doesNotMatch(serialized, /src\/result\.ts|@@ -1 \+1 @@/u)

  expectApiError('RELATIONSHIP_MISMATCH', 'response.result.diagramExecution', () => (
    materializeStrongFlowDeliverySuccess(
      request,
      draftDelivery(),
      { ...liveDiagramExecution(), deliveryRevision: 2 },
    )
  ))
  expectApiError('INVALID_RESPONSE', 'response.result.diagramExecution', () => (
    materializeStrongFlowDeliverySuccess(
      request,
      draftDelivery(),
      {
        ...liveDiagramExecution(),
        details: { filePath: 'src/result.ts', hunk: '@@ -1 +1 @@' },
      },
    )
  ))
})

test('Delivery API rejects invalid response error codes and response shapes', () => {
  expectApiError('INVALID_RESPONSE', 'response.error.code', () => (
    parseStrongFlowDeliveryResponse({
      schemaVersion: STRONGFLOW_DELIVERY_API_SCHEMA_VERSION,
      requestId: 'request-failure',
      operation: 'getDeliveryProjection',
      ok: false,
      error: {
        code: 'UNKNOWN_FAILURE',
        message: 'Unknown response.',
        currentRevision: null,
      },
    })
  ))
  expectApiError('INVALID_RESPONSE', 'response.operation', () => (
    parseStrongFlowDeliveryResponse({
      schemaVersion: STRONGFLOW_DELIVERY_API_SCHEMA_VERSION,
      requestId: 'request-failure',
      operation: 'job.status',
      ok: false,
      error: {
        code: 'INVALID_REQUEST',
        message: 'Invalid response.',
        currentRevision: null,
      },
    })
  ))
})

test('Delivery API never accepts an Attention response that echoes its proof', () => {
  const request = materializeStrongFlowDeliveryRequest(
    'resolveAttention',
    'request-proof-leak',
    {
      deliveryId,
      expectedRevision: 1,
      attentionItemId: 'attention-api-decision',
      status: 'resolved',
      resolution: 'Approve the reviewed plan.',
      remediation: null,
      channel: 'local-ui',
      authentication: { scheme: 'local-session', proof },
    },
  )
  const response = materializeStrongFlowDeliveryFailure({
    requestId: request.requestId,
    operation: request.operation,
    code: 'AUTHENTICATION_FAILED',
    message: `Rejected ${proof}`,
  })
  expectApiError('RELATIONSHIP_MISMATCH', 'response', () => (
    parseStrongFlowDeliveryResponseForRequest(request, response)
  ))
})
