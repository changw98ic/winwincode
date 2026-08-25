import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { join, resolve } from 'node:path'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')
const schemaRoot = join(root, 'schema', 'winwincode', 'v1')

function json(name) {
  return JSON.parse(readFileSync(join(schemaRoot, name), 'utf8'))
}

function branchConstants(schema, unionName, propertyName) {
  return schema.$defs[unionName].oneOf.map(branch => {
    const name = branch.$ref.split('/').at(-1)
    const definition = schema.$defs[name]
    const object = definition.allOf?.find(item => item.type === 'object') ?? definition
    if (object.properties?.[propertyName]?.const !== undefined) {
      return object.properties[propertyName].const
    }
    const nested = object.oneOf?.map(item => {
      const nestedName = item.$ref.split('/').at(-1)
      return schema.$defs[nestedName].properties[propertyName].const
    }) ?? []
    assert.equal(new Set(nested).size, 1, `${name} needs one nested ${propertyName}`)
    return nested[0]
  })
}

function sorted(values) {
  return [...values].sort((left, right) => left.localeCompare(right))
}

function propertyNames(value, names = new Set()) {
  if (Array.isArray(value)) {
    for (const entry of value) propertyNames(entry, names)
    return names
  }
  if (value === null || typeof value !== 'object') return names
  for (const name of Object.keys(value.properties ?? {})) names.add(name)
  for (const entry of Object.values(value)) propertyNames(entry, names)
  return names
}

test('delivery.get has one closed StrongFlow detail while DeliveryPage stays compact', () => {
  const domain = json('domain.schema.json')
  const http = json('control-plane-http.schema.json')

  assert.equal(http.$defs.DeliveryPage.properties.items.items.$ref,
    './domain.schema.json#/$defs/DeliveryProjection')
  assert.deepEqual(sorted(Object.keys(domain.$defs.DeliveryProjection.properties)), sorted([
    'schemaVersion',
    'deliveryId',
    'revision',
    'ownership',
    'title',
    'status',
    'taskCounts',
    'activeStageRunId',
    'openAttentionCount',
    'updatedAt',
  ]))

  assert.equal(
    http.$defs.DeliveryGetResultResponse.allOf[1].properties.result.$ref,
    '#/$defs/DeliveryDetailProjection',
  )
  assert.equal(http.$defs.QueryResult, undefined)

  const detail = http.$defs.DeliveryDetailProjection
  assert.equal(detail.additionalProperties, false)
  assert.equal(detail.properties.kind.const, 'delivery_detail')
  assert.deepEqual(detail.required, [
    'kind',
    'schemaVersion',
    'readCursor',
    'deliveryId',
    'deliveryRevision',
    'ownership',
    'status',
    'requirements',
    'solutionReview',
    'stages',
    'tasks',
    'attention',
    'evidence',
    'currentCandidate',
    'verdict',
    'publication',
  ])
  for (const name of [
    'DeliveryRequirementsProjection',
    'SolutionReviewProjection',
    'DeliveryStageProjection',
    'DeliveryTaskDetailProjection',
    'DeliveryAttentionProjection',
    'DeliveryEvidenceProjection',
    'FrozenCandidateSummaryProjection',
    'DeliveryVerdictProjection',
    'PublicationProjection',
  ]) assert.ok(http.$defs[name], name)

  const binding = http.$defs.DeliveryStageSessionBindingProjection
  assert.ok(binding.required.includes('workerSessionId'))
  assert.ok(binding.required.includes('codexThreadId'))
  assert.deepEqual(binding.properties.workerSessionId.oneOf.at(-1), { type: 'null' })
  assert.deepEqual(binding.properties.codexThreadId.oneOf.at(-1), { type: 'null' })
  assert.deepEqual(binding.oneOf, [
    { properties: { codexThreadId: { type: 'null' } } },
    {
      properties: {
        workerSessionId: {
          $ref: './domain.schema.json#/$defs/WorkerSessionId',
        },
        codexThreadId: {
          $ref: './domain.schema.json#/$defs/CodexThreadId',
        },
      },
    },
  ])
  assert.deepEqual(http.$defs.DeliveryStageProjection.oneOf[0], {
    properties: {
      actorType: { const: 'human' },
      sessionBinding: { type: 'null' },
    },
  })
  assert.deepEqual(http.$defs.DeliveryStageProjection.oneOf[1], {
    properties: {
      actorType: { const: 'codex' },
      sessionBinding: { $ref: '#/$defs/DeliveryStageSessionBindingProjection' },
    },
  })
  assert.deepEqual(http['x-winwincode-semantics'].deliveryStageSessionBinding, {
    partialBindingFields: [
      'bindingId',
      'productSessionId',
      'executionJobId',
      'boundAt',
    ],
    codexThreadRequiresWorkerSession: true,
    humanStageBinding: 'must_be_null',
    runtimeSessionBinding: 'worker_and_codex_required',
  })
})

test('current solution review and publication expose exact authority joins', () => {
  const http = json('control-plane-http.schema.json')
  assert.equal(http.$defs.ApprovedSolutionProjection, undefined)
  const review = http.$defs.SolutionReviewProjection
  assert.equal(review.additionalProperties, false)
  assert.deepEqual(review.required, [
    'deliveryId',
    'deliverySpecId',
    'deliverySpecRevision',
    'planningStageRunId',
    'planningSessionBindingId',
    'reviewStageRunId',
    'attentionItemId',
    'reviewSetSha256',
    'reviewStatus',
    'decision',
    'comments',
    'requestedChanges',
    'reviewerId',
    'reviewedAt',
    'solutionId',
    'summary',
    'approach',
    'components',
    'connections',
    'architectureDiagram',
    'processDiagram',
    'risks',
    'unresolvedItems',
    'taskProposals',
  ])
  assert.equal(review.properties.reviewSessionBindingId, undefined)
  assert.equal(review.properties.reviewStatus.$ref, '#/$defs/SolutionReviewStatus')
  assert.deepEqual(http.$defs.SolutionReviewStatus.enum, [
    'pending',
    'approved',
    'changes_requested',
    'rejected',
  ])
  assert.equal(review.properties.decision.oneOf[0].$ref,
    '#/$defs/SolutionReviewDecision')
  assert.deepEqual(http.$defs.SolutionReviewDecision.enum, [
    'approve',
    'request_changes',
    'reject',
  ])
  assert.equal(review.properties.reviewSetSha256.$ref,
    './domain.schema.json#/$defs/Sha256Digest')
  assert.equal(review.properties.taskProposals.minItems, 1)
  assert.equal(review.properties.taskProposals.items.$ref,
    '#/$defs/DeliveryTaskProposalProjection')
  const taskProposal = http.$defs.DeliveryTaskProposalProjection
  assert.equal(taskProposal.additionalProperties, false)
  assert.deepEqual(taskProposal.required, [
    'id',
    'title',
    'goal',
    'acceptanceCriterionIds',
    'blockedByTaskIds',
  ])
  assert.equal(taskProposal.properties.ownerActorId, undefined)
  assert.equal(taskProposal.properties.title.maxLength, 256)
  assert.equal(review.properties.taskProposals.maxItems, 200)
  assert.equal(taskProposal.properties.acceptanceCriterionIds.maxItems, 200)
  assert.equal(taskProposal.properties.blockedByTaskIds.maxItems, 200)
  assert.equal(review.oneOf.length, 4)
  assert.deepEqual(review.oneOf.map(branch => branch.properties.reviewStatus.const), [
    'pending',
    'approved',
    'changes_requested',
    'rejected',
  ])
  assert.equal(review.oneOf[0].properties.reviewerId.type, 'null')
  assert.equal(review.oneOf[0].properties.reviewedAt.type, 'null')
  for (const branch of review.oneOf.slice(1)) {
    assert.equal(branch.properties.reviewerId.$ref,
      './domain.schema.json#/$defs/ActorId')
    assert.equal(branch.properties.reviewedAt.$ref,
      './domain.schema.json#/$defs/Instant')
  }
  assert.equal(review.oneOf[2].properties.requestedChanges.minItems, 1)
  assert.equal(review.oneOf[3].properties.comments, undefined)
  assert.equal(review.oneOf[3].properties.requestedChanges.type, 'null')

  const promotion = http.$defs.DeliveryApproveTaskBreakdownPayload
  assert.deepEqual(promotion.required, ['deliveryId', 'reviewSetSha256'])
  assert.deepEqual(Object.keys(promotion.properties), ['deliveryId', 'reviewSetSha256'])
  assert.equal(promotion.properties.tasks, undefined)
  assert.equal(promotion.properties.reviewSetSha256.$ref,
    './domain.schema.json#/$defs/Sha256Digest')
  assert.deepEqual(http['x-winwincode-semantics'].solutionReview, {
    digestIncludesOrderedTaskProposals: true,
    pendingProjectionAllowed: true,
    settledReviewerRequired: true,
    pendingReviewerMustBeNull: true,
    pendingDecisionFieldsMustBeNull: true,
    changesRequestedRequiresNonEmptyRequestedChanges: true,
    commentsAreBoundedNullableTypedDecisionField: true,
    reviewSetDigestExcludesDecision: true,
    plannerSuppliedTaskOwnerAllowed: false,
    promotedTaskOwner: null,
    promotedTaskInitialStatus: 'pending',
    taskPromotionStatus: 'approved',
    rawAttentionContextReturned: false,
    rawAttentionResolutionReturned: false,
  })

  const publication = http.$defs.PublicationProjection
  assert.equal(publication.additionalProperties, false)
  for (const field of [
    'id',
    'revision',
    'deliveryId',
    'deliverySpecId',
    'deliverySpecRevision',
    'candidateRef',
    'deliveryVerdictId',
    'verdictStatus',
    'approvalAttentionItemId',
    'approvedBy',
    'approvedAt',
    'publicationSetSha256',
    'target',
    'state',
    'resourceRef',
    'updatedAt',
  ]) assert.ok(publication.required.includes(field), field)
  assert.equal(publication.properties.verdictStatus.const, 'pass')
  assert.equal(publication.properties.approvedBy.$ref,
    './domain.schema.json#/$defs/ActorId')
  assert.equal(publication.properties.publicationSetSha256.$ref,
    './domain.schema.json#/$defs/Sha256Digest')
  assert.deepEqual(publication.properties.resourceRef.oneOf, [
    { $ref: '#/$defs/PublicationResourceRef' },
    { type: 'null' },
  ])
  const publicationResource = http.$defs.PublicationResourceRef
  assert.equal(publicationResource.additionalProperties, false)
  assert.deepEqual(publicationResource.required, ['kind', 'repository', 'number'])
  assert.deepEqual(http.$defs.PublicationResourceKind.enum, [
    'github_issue',
    'github_pull_request',
  ])
  assert.equal(publicationResource.properties.repository.$ref,
    '#/$defs/GitHubRepositorySlug')
  assert.equal(publicationResource.properties.number.minimum, 1)
  assert.equal(publicationResource.properties.webUrl, undefined)
  assert.deepEqual(http['x-winwincode-semantics'].strongFlowPublicationJoin, {
    snapshot: 'delivery.get',
    requiresExactMatch: [
      'deliveryId',
      'deliverySpecId',
      'deliverySpecRevision',
      'candidateRef',
      'deliveryVerdictId',
      'verdictStatus=pass',
      'approvalAttentionItemId',
      'target',
      'resourceRef.repository=target.repository',
      'publicationSetSha256',
    ],
    mismatch: 'fail_entire_read_cut',
    resourceRef: {
      allowedKinds: ['github_issue', 'github_pull_request'],
      arbitraryUrlAllowed: false,
      rawProviderPayloadAllowed: false,
    },
  })

  for (const field of ['assignedTo', 'resolvedBy']) {
    assert.deepEqual(http.$defs.DeliveryAttentionProjection.properties[field].oneOf, [
      { $ref: './domain.schema.json#/$defs/ActorId' },
      { type: 'null' },
    ])
  }
})

test('delivery and runtime reads share one server-issued bounded read cursor', () => {
  const domain = json('domain.schema.json')
  const http = json('control-plane-http.schema.json')
  const examples = json('examples/control-plane-http.examples.json')
  const cursor = domain.$defs.StrongFlowReadCursor

  assert.equal(cursor.additionalProperties, false)
  assert.deepEqual(cursor.required, [
    'token',
    'scope',
    'deliveryId',
    'deliveryRevision',
    'runtimeLedgerRevision',
    'runtimeAcceptedSequence',
    'publicationRevision',
    'eventCursor',
  ])
  assert.equal(cursor.properties.scope.$ref, '#/$defs/RepositoryScope')
  assert.equal(cursor.properties.deliveryId.$ref, '#/$defs/DeliveryId')
  assert.equal(cursor.properties.deliveryRevision.$ref, '#/$defs/Revision')
  assert.equal(cursor.properties.runtimeLedgerRevision.$ref, '#/$defs/Revision')
  assert.equal(cursor.properties.runtimeAcceptedSequence.minimum, 0)
  assert.equal(cursor.properties.publicationRevision.$ref, '#/$defs/Revision')
  assert.equal(
    cursor.properties.eventCursor.$ref,
    '#/$defs/DeliveryEventReadCursor',
  )
  assert.equal(cursor.properties.generatedAt, undefined)

  const deliveryDetail = http.$defs.DeliveryDetailProjection
  assert.equal(deliveryDetail.properties.readCursor.$ref,
    './domain.schema.json#/$defs/StrongFlowReadCursor')

  const deliveryGet = http.$defs.DeliveryGetParameters
  assert.equal(deliveryGet.properties.atCursor.$ref,
    './domain.schema.json#/$defs/StrongFlowReadCursor')
  assert.equal(deliveryGet.required.includes('atCursor'), false)

  const deliveryRuntime = http.$defs.DeliveryStageRuntimeProjectionGetParameters
  assert.ok(deliveryRuntime.required.includes('atCursor'))
  assert.equal(deliveryRuntime.properties.atCursor.$ref,
    './domain.schema.json#/$defs/StrongFlowReadCursor')
  assert.equal(
    http.$defs.ProductSessionRuntimeProjectionGetParameters.properties.atCursor,
    undefined,
  )

  const runtime = domain.$defs.RuntimeProjectionSnapshot
  assert.ok(runtime.required.includes('readCursor'))
  assert.ok(runtime.required.includes('eventCursor'))
  assert.deepEqual(runtime.properties.readCursor.oneOf, [
    { $ref: '#/$defs/StrongFlowReadCursor' },
    { type: 'null' },
  ])
  assert.equal(runtime.properties.eventCursor.$ref, '#/$defs/RuntimeProjectionEventCursor')
  assert.deepEqual(domain.$defs.RuntimeProjectionEventCursor.oneOf, [
    { $ref: '#/$defs/ProductSessionEventReadCursor' },
    { $ref: '#/$defs/DeliveryEventReadCursor' },
  ])
  assert.deepEqual(http['x-winwincode-semantics'].strongFlowReadCut, {
    bootstrapQuery: 'delivery.get',
    followupQuery: 'runtime.projection.get',
    requestCursorField: 'atCursor',
    resultCursorField: 'readCursor',
    exactCoordinates: [
      'scope',
      'deliveryId',
      'deliveryRevision',
      'runtimeLedgerRevision',
      'runtimeAcceptedSequence',
      'publicationRevision',
      'eventCursor',
    ],
    deliveryStageRuntimeRequiresReturnedCursor: true,
    cursorAuthority: 'server_issued_and_authenticated',
    requestMustMatchCursor: [
      'envelope.scope',
      'parameters.deliveryId',
    ],
    pairedResultsMustReturnIdenticalCursor: true,
    mismatch: 'reject_entire_pair_and_reload',
    expiredCursorError: 'READ_CURSOR_EXPIRED',
    expiredCursorAction:
      'discard_all_partial_state_and_restart_delivery_get_without_at_cursor',
    malformedCursorError: 'INVALID_REQUEST',
    foreignScopeCursorError: 'PERMISSION_DENIED',
    factAdapterUnavailableError: 'TRUSTED_FACTS_UNAVAILABLE',
    generatedTimeIsCursor: false,
    eventCursorResultPaths: {
      'delivery.get': 'result.readCursor.eventCursor',
      'runtime.projection.get': 'result.eventCursor',
    },
    webSocketHandoff: 'subscribe.startAt_must_equal_result_event_cursor',
    deliveryRuntimeEventCursorMustEqualReadCursor: true,
    productSessionEventCursorMustMatch: [
      'envelope.scope',
      'parameters.productSessionId',
    ],
  })

  const mismatch = examples.strongFlowReadCutMismatch
  assert.notDeepEqual(mismatch.deliveryCursor, mismatch.runtimeCursor)
  assert.equal(mismatch.deliveryCursor.runtimeAcceptedSequence, 42)
  assert.equal(mismatch.runtimeCursor.runtimeAcceptedSequence, 43)
  assert.equal(mismatch.expected.error.code, 'REVISION_CONFLICT')
  assert.equal(mismatch.expected.error.details.field, 'readCursor')

  assert.equal(examples.readCursorExpired.error.code, 'READ_CURSOR_EXPIRED')
  assert.equal(examples.readCursorExpired.error.retryable, true)
  assert.equal(
    examples.readCursorExpired.error.details.action,
    'restart_strongflow_read_without_cursor',
  )
})

test('runtime.projection.get returns bounded typed sessions and only a live Diff summary', () => {
  const domain = json('domain.schema.json')
  const snapshot = domain.$defs.RuntimeProjectionSnapshot
  assert.ok(snapshot.properties.sessions)
  assert.equal(snapshot.properties.items, undefined)
  assert.equal(snapshot.properties.sessions.items.$ref, '#/$defs/RuntimeSessionProjection')

  const session = domain.$defs.RuntimeSessionProjection
  assert.equal(session.additionalProperties, false)
  for (const field of [
    'sessionBindingId',
    'stageRunId',
    'deliveryTaskId',
    'productSessionId',
    'workerSessionId',
    'codexThreadId',
    'executionJobId',
    'leaseId',
    'attempt',
    'fencingToken',
    'asOfSequence',
    'plan',
    'agents',
    'agentEdges',
    'activities',
    'usage',
    'recovery',
    'diffSummary',
  ]) assert.ok(session.required.includes(field), field)

  const diff = domain.$defs.RuntimeDiffSummaryProjection
  assert.deepEqual(sorted(Object.keys(diff.properties)), sorted([
    'changedFileCount',
    'additions',
    'deletions',
    'detailsVisible',
    'sourceRef',
  ]))
  assert.equal(diff.properties.detailsVisible.const, false)

  const forbidden = [
    'apiKey',
    'authorization',
    'credential',
    'providerRequest',
    'providerResponse',
    'rawRuntimeLog',
    'stdout',
    'stderr',
    'toolPayload',
    'changedFiles',
    'filePath',
    'hunk',
    'hunkContent',
    'unifiedDiff',
  ]
  const publicFields = propertyNames({
    RuntimeProjectionSnapshot: snapshot,
    RuntimeSessionProjection: session,
    RuntimeDiffSummaryProjection: diff,
  })
  for (const field of forbidden) assert.equal(publicFields.has(field), false, field)
})

test('WebSocket uses invalidation and reset names both canonical reload queries', () => {
  const events = json('control-plane-events.schema.json')
  const eventTypes = branchConstants(events, 'ControlPlaneWebSocketEventPayload', 'type')
  assert.ok(eventTypes.includes('runtime-projection.invalidated.v1'))
  assert.equal(eventTypes.includes('runtime-projection.appended.v1'), false)

  const invalidated = events.$defs.ControlPlaneWebSocketRuntimeProjectionInvalidatedEvent
  assert.deepEqual(invalidated.oneOf, [
    {
      $ref: '#/$defs/ControlPlaneWebSocketProductSessionRuntimeProjectionInvalidatedEvent',
    },
    {
      $ref: '#/$defs/ControlPlaneWebSocketDeliveryStageRuntimeProjectionInvalidatedEvent',
    },
  ])
  const productInvalidated =
    events.$defs.ControlPlaneWebSocketProductSessionRuntimeProjectionInvalidatedEvent
  assert.deepEqual(productInvalidated.required, [
    'type',
    'scopeKind',
    'productSessionId',
    'projectionRevision',
    'lastProjectionSequence',
    'reloadQueries',
  ])
  assert.equal(productInvalidated.properties.scopeKind.const, 'product-session')
  assert.equal(productInvalidated.properties.deliveryId, undefined)
  assert.equal(productInvalidated.properties.stageRunId, undefined)
  assert.deepEqual(productInvalidated.properties.reloadQueries.prefixItems, [
    { $ref: '#/$defs/ControlPlaneWebSocketRuntimeProjectionGetReloadQuery' },
  ])
  assert.equal(productInvalidated.properties.reloadQueries.minItems, 1)
  assert.equal(productInvalidated.properties.reloadQueries.maxItems, 1)

  const deliveryInvalidated =
    events.$defs.ControlPlaneWebSocketDeliveryStageRuntimeProjectionInvalidatedEvent
  assert.deepEqual(deliveryInvalidated.required, [
    'type',
    'scopeKind',
    'productSessionId',
    'deliveryId',
    'stageRunId',
    'projectionRevision',
    'lastProjectionSequence',
    'reloadQueries',
  ])
  assert.equal(deliveryInvalidated.properties.scopeKind.const, 'delivery-stage')
  assert.deepEqual(deliveryInvalidated.properties.reloadQueries.prefixItems, [
    { $ref: '#/$defs/ControlPlaneWebSocketDeliveryGetReloadQuery' },
    { $ref: '#/$defs/ControlPlaneWebSocketRuntimeProjectionGetReloadQuery' },
  ])
  assert.equal(deliveryInvalidated.properties.reloadQueries.minItems, 2)
  assert.equal(deliveryInvalidated.properties.reloadQueries.maxItems, 2)
  for (const branch of [productInvalidated, deliveryInvalidated]) {
    for (const field of ['summary', 'detail', 'runtimeItem', 'toolPayload', 'unifiedDiff']) {
      assert.equal(branch.properties[field], undefined, field)
    }
  }

  const reset = events.$defs.ControlPlaneWebSocketResetRequiredFrame
  assert.equal(reset.required.includes('reloadQueries'), false)
  assert.equal(reset.properties.reloadQueries, undefined)
})

test('ExecutionPort binds one CodexThread before accepting runtime events', () => {
  const execution = json('execution-port.schema.json')
  const kinds = branchConstants(execution, 'ExecutionPortMessage', 'kind')
  assert.ok(kinds.includes('session.binding'))

  const binding = execution.$defs.SessionBindingMessage
  assert.equal(binding['x-direction'], 'worker-to-control-plane')
  assert.equal(binding['x-authority'], 'lease-write')
  for (const field of [
    'schemaVersion',
    'messageId',
    'sentAt',
    'lease',
    'productSessionId',
    'workerSessionId',
    'codexThreadId',
    'boundAt',
  ]) assert.ok(binding.required.includes(field), field)

  const runtime = execution.$defs.RuntimeEventMessage
  assert.ok(runtime.required.includes('codexThreadId'))
  assert.equal(runtime.properties.codexThreadId.$ref,
    './domain.schema.json#/$defs/CodexThreadId')
  assert.deepEqual(execution['x-winwincode-semantics'].sessionBinding, {
    authority: 'accepted_session.binding',
    exactIdentity: [
      'productSessionId',
      'workerSessionId',
      'codexThreadId',
      'executionJobId',
      'attempt',
      'leaseId',
      'fencingToken',
    ],
    duplicate: 'same_identity_is_idempotent_changed_identity_is_conflict',
    runtimeBeforeBinding: 'reject_before_persist_and_projection',
    runtimeThreadMismatch: 'reject_before_persist_and_projection',
  })
})

test('canonical schemas carry generated-client route and event metadata', () => {
  const http = json('control-plane-http.schema.json')
  const events = json('control-plane-events.schema.json')
  assert.deepEqual(http['x-winwincode-transports'].generatedClient, {
    transport: 'http',
    commandEndpoint: '/api/v1/commands',
    queryEndpoint: '/api/v1/queries',
    strongFlowRead: {
      bootstrapQuery: 'delivery.get',
      followupQuery: 'runtime.projection.get',
      requestCursorField: 'atCursor',
      resultCursorField: 'readCursor',
      failureMode: 'discard_partial_pair',
    },
    queries: [
      {
        query: 'delivery.get',
        requestDefinition: 'DeliveryGetQuery',
        resultDefinition: 'DeliveryDetailProjection',
      },
      {
        query: 'runtime.projection.get',
        requestDefinition: 'RuntimeProjectionGetQuery',
        resultDefinition: 'RuntimeProjectionSnapshot',
      },
    ],
    authentication: {
      anonymousAllowed: false,
      sessionCookie: { location: 'cookie', name: 'wwc_session' },
      bearerAuth: {
        location: 'header',
        name: 'Authorization',
        scheme: 'Bearer',
        format: 'JWT',
      },
      credentialsInQueryAllowed: false,
      credentialsInBodyAllowed: false,
    },
    responseCorrelation: {
      command: 'command',
      query: 'query',
      mismatch: 'reject',
    },
    snapshotHandoff: {
      deliveryGetCursorPath: 'result.readCursor.eventCursor',
      runtimeProjectionGetCursorPath: 'result.eventCursor',
      subscribeStartAtField: 'startAt',
    },
  })
  assert.deepEqual(events['x-winwincode-transports'].generatedClient, {
    transport: 'websocket',
    endpoint: '/api/v1/events',
    updateMode: 'invalidation_then_http_reload',
    runtimeEvent: 'runtime-projection.invalidated.v1',
    invalidationReloadQueries: {
      'product-session': ['runtime.projection.get'],
      'delivery-stage': ['delivery.get', 'runtime.projection.get'],
    },
    resetStrategy: 'discard_then_full_reload_by_original_subscription_stream',
    authentication: {
      upgradeOnly: true,
      sessionCookie: { location: 'cookie', name: 'wwc_session' },
      bearerAuth: {
        location: 'header',
        name: 'Authorization',
        scheme: 'Bearer',
        format: 'JWT',
      },
      credentialsInUrlQueryAllowed: false,
      credentialsInFramesAllowed: false,
    },
    snapshotHandoff: {
      httpCursorField: 'eventCursor',
      subscribeCursorField: 'startAt',
      cursorMustMatchSubscription: ['scope', 'stream'],
      acceptedBaselineFrame: 'transport.subscription-accepted.v1',
      firstEventSequence: 'accepted.cursor.sequence + 1',
      authorizationEpochBaseline: 'accepted.authorizationEpoch',
      retentionLossFrame: 'transport.reset-required.v1',
      retentionLossCloseCode: 4409,
    },
  })

  const generated = readFileSync(join(
    root,
    'apps',
    'web',
    'src',
    'generated',
    'contracts.ts',
  ), 'utf8')
  assert.match(generated, /export const WINWINCODE_CLIENT_METADATA =/u)
  assert.match(generated, /"runtime-projection\.invalidated\.v1"/u)
  assert.match(generated, /"DeliveryDetailProjection"/u)
  assert.match(generated, /export type SolutionReviewProjection =/u)
  assert.match(generated, /export type StrongFlowReadCursor =/u)
  assert.match(generated, /export type EventReadCursor =/u)
  assert.match(
    generated,
    /readonly "eventId": null\n\s+readonly "sequence": 0\n\} \| \{\n\s+readonly "eventId": ControlPlaneEventId\n\s+readonly "sequence": number/u,
  )
  assert.doesNotMatch(generated, /export type ApprovedSolutionProjection =/u)
  assert.match(generated, /"failureMode":\s*"discard_partial_pair"/u)
})
