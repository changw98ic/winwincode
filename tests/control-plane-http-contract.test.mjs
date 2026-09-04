import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'
import { join, resolve } from 'node:path'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')
const schemaRoot = join(root, 'schema', 'winwincode', 'v1')

const COMMANDS = Object.freeze([
  'session.create',
  'chat.submit',
  'input.respond',
  'session.cancel',
  'session.close',
  'delivery.create',
  'delivery.update_spec',
  'delivery.approve_task_breakdown',
  'delivery.advance',
  'delivery.resolve_attention',
  'delivery.submit_verdict',
  'settings.update',
  'credential.reference.create',
  'credential.reference.rotate',
  'credential.reference.revoke',
  'credential.reference.delete',
  'approval.decide',
  'worker.drain',
  'worker.enable',
  'publication.publish',
  'publication.cancel',
  'enterprise.organization.update',
  'enterprise.membership.update',
  'enterprise.team.update',
  'enterprise.role.update',
  'enterprise.project_repository.update',
  'enterprise.policy.update',
  'enterprise.fleet.update',
  'enterprise.integration.update',
  'enterprise.identity.update',
  'collaboration.notification.ack',
  'collaboration.presence.update',
])

const QUERIES = Object.freeze([
  'session.list',
  'session.get',
  'session.messages.list',
  'session.interactions.list',
  'runtime.projection.get',
  'delivery.list',
  'delivery.get',
  'candidate.list',
  'candidate.review.get',
  'candidate.files.list',
  'candidate.diff.get',
  'evidence.get',
  'evidence.artifact.content.get',
  'settings.get',
  'model.route.availability.list',
  'credential.reference.list',
  'credential.reference.get',
  'approval.list',
  'approval.get',
  'worker.list',
  'worker.get',
  'publication.list',
  'publication.get',
  'enterprise.organization.list',
  'enterprise.membership.list',
  'enterprise.team.list',
  'enterprise.role.list',
  'enterprise.project.list',
  'enterprise.policy.list',
  'enterprise.fleet.list',
  'enterprise.usage.list',
  'enterprise.audit.list',
  'enterprise.integration.list',
  'enterprise.identity.list',
  'collaboration.activity.list',
  'collaboration.notification.list',
  'collaboration.presence.list',
])

const ERROR_STATUS = Object.freeze({
  INVALID_REQUEST: 400,
  AUTHENTICATION_REQUIRED: 401,
  PERMISSION_DENIED: 403,
  RESOURCE_NOT_FOUND: 404,
  IDEMPOTENCY_CONFLICT: 409,
  REVISION_CONFLICT: 409,
  READ_CURSOR_EXPIRED: 409,
  CANDIDATE_STALE: 409,
  WRONG_STATE: 409,
  RATE_LIMITED: 429,
  INTERNAL_ERROR: 500,
  SERVICE_UNAVAILABLE: 503,
  TRUSTED_FACTS_UNAVAILABLE: 503,
})

async function json(...parts) {
  return JSON.parse(await readFile(join(schemaRoot, ...parts), 'utf8'))
}

function collectRefs(value, refs = []) {
  if (Array.isArray(value)) {
    for (const entry of value) collectRefs(entry, refs)
    return refs
  }
  if (typeof value !== 'object' || value === null) return refs
  for (const [key, entry] of Object.entries(value)) {
    if (key === '$ref') refs.push(entry)
    else collectRefs(entry, refs)
  }
  return refs
}

function localDefinition(schema, ref) {
  const prefix = '#/$defs/'
  assert.match(ref, /^#\/\$defs\/[A-Za-z][A-Za-z0-9]*$/u)
  return schema.$defs[ref.slice(prefix.length)]
}

function specializationNames(schema, unionName, discriminator) {
  return schema.$defs[unionName].oneOf.map(({ $ref }) => {
    const definition = localDefinition(schema, $ref)
    const specialization = definition.allOf.find(part => part.type === 'object')
    assert.ok(specialization, `${$ref} must specialize a shared envelope`)
    return specialization.properties[discriminator].const
  })
}

test('HTTP contract specializes every accepted command without copying domain primitives', async () => {
  const schema = await json('control-plane-http.schema.json')

  assert.equal(schema.$schema, 'https://json-schema.org/draft/2020-12/schema')
  assert.equal(
    schema.$id,
    'https://schemas.winwincode.dev/winwincode/v1/control-plane-http.schema.json',
  )
  assert.deepEqual(specializationNames(schema, 'CommandRequest', 'command'), COMMANDS)

  const forbiddenCopies = [
    'ApiTokenId',
    'RequestId',
    'Revision',
    'Actor',
    'Scope',
    'OrganizationId',
    'WorkspaceId',
    'ProjectId',
    'RepositoryId',
    'DeliveryId',
    'ExecutionJobId',
    'ExternalIdentityId',
    'InputRequestId',
    'ProductSessionId',
    'ServiceAccountId',
    'WorkerId',
  ]
  for (const name of forbiddenCopies) assert.equal(schema.$defs[name], undefined)

  const refs = collectRefs(schema)
  assert.ok(refs.includes('./domain.schema.json#/$defs/CommandEnvelope'))
  for (const name of forbiddenCopies) {
    assert.ok(
      refs.includes(`./domain.schema.json#/$defs/${name}`),
      `HTTP contract must reuse domain ${name}`,
    )
  }
  assert.equal(
    refs.some(ref => typeof ref !== 'string'
      || (!ref.startsWith('#/$defs/')
        && !ref.startsWith('./domain.schema.json#/$defs/')
        && !ref.startsWith('./control-plane-http.schema.json#/$defs/'))),
    false,
  )
})

test('HTTP query contract covers every current read surface with an opaque stable cursor', async () => {
  const schema = await json('control-plane-http.schema.json')
  assert.deepEqual(specializationNames(schema, 'QueryRequest', 'query'), QUERIES)

  const queryEnvelope = schema.$defs.QueryEnvelope
  assert.deepEqual(queryEnvelope.required, [
    'schemaVersion',
    'requestId',
    'query',
    'actor',
    'scope',
    'parameters',
    'page',
  ])
  assert.equal(queryEnvelope.properties.actor.$ref, './domain.schema.json#/$defs/Actor')
  assert.equal(queryEnvelope.properties.scope.$ref, './domain.schema.json#/$defs/Scope')
  assert.equal(queryEnvelope.properties.page.$ref, '#/$defs/PageRequest')
  assert.equal(schema.$defs.OpaqueCursor.type, 'string')
  assert.equal(schema.$defs.PageRequest.properties.limit.maximum, 200)
  assert.equal(schema.$defs.PageInfo.properties.nextCursor.oneOf[1].type, 'null')
  assert.deepEqual(
    specializationNames(schema, 'QueryResultResponse', 'query'),
    QUERIES,
  )
  assert.equal(schema.$defs.QueryResult, undefined)
  const queryResultRefs = Object.fromEntries(
    schema.$defs.QueryResultResponse.oneOf.map(({ $ref }) => {
      const specialization = localDefinition(schema, $ref)
      const constraint = specialization.allOf.at(1)
      return [constraint.properties.query.const, constraint.properties.result.$ref]
    }),
  )
  assert.deepEqual(queryResultRefs, {
    'session.list': '#/$defs/ProductSessionPage',
    'session.get': '#/$defs/ProductSessionProjection',
    'session.messages.list': '#/$defs/ChatMessagePage',
    'session.interactions.list': '#/$defs/ChatInteractionPage',
    'runtime.projection.get': './domain.schema.json#/$defs/RuntimeProjectionSnapshot',
    'delivery.list': '#/$defs/DeliveryPage',
    'delivery.get': '#/$defs/DeliveryDetailProjection',
    'candidate.list': '#/$defs/CandidateHistoryPage',
    'candidate.review.get': '#/$defs/CandidateHistoricalReviewProjection',
    'candidate.files.list': '#/$defs/CandidateFilePage',
    'candidate.diff.get': '#/$defs/CandidateDiffChunkProjection',
    'evidence.get': '#/$defs/EvidenceDetailProjection',
    'evidence.artifact.content.get': '#/$defs/EvidenceArtifactContentResult',
    'settings.get': '#/$defs/SettingsProjection',
    'model.route.availability.list': '#/$defs/ModelRouteAvailabilityPage',
    'credential.reference.list': '#/$defs/CredentialReferencePage',
    'credential.reference.get': '#/$defs/CredentialReferenceProjection',
    'approval.list': '#/$defs/ApprovalPage',
    'approval.get': '#/$defs/ApprovalProjection',
    'worker.list': '#/$defs/WorkerPage',
    'worker.get': '#/$defs/WorkerProjection',
    'publication.list': '#/$defs/PublicationPage',
    'publication.get': '#/$defs/PublicationDetailProjection',
    'enterprise.organization.list': '#/$defs/EnterpriseOrganizationPage',
    'enterprise.membership.list': '#/$defs/EnterpriseMembershipPage',
    'enterprise.team.list': '#/$defs/EnterpriseTeamPage',
    'enterprise.role.list': '#/$defs/EnterpriseRolePage',
    'enterprise.project.list': '#/$defs/EnterpriseProjectRepositoryPage',
    'enterprise.policy.list': '#/$defs/EnterprisePolicyPage',
    'enterprise.fleet.list': '#/$defs/EnterpriseFleetPage',
    'enterprise.usage.list': '#/$defs/EnterpriseUsagePage',
    'enterprise.audit.list': '#/$defs/EnterpriseAuditPage',
    'enterprise.integration.list': '#/$defs/EnterpriseIntegrationPage',
    'enterprise.identity.list': '#/$defs/EnterpriseIdentityPage',
    'collaboration.activity.list': '#/$defs/CollaborationActivityPage',
    'collaboration.notification.list': '#/$defs/CollaborationNotificationPage',
    'collaboration.presence.list': '#/$defs/CollaborationPresencePage',
  })
  assert.deepEqual(
    [
      'ProductSessionPage',
      'ChatInteractionPage',
      'DeliveryPage',
      'CandidateHistoryPage',
      'CandidateFilePage',
      'ModelRouteAvailabilityPage',
      'CredentialReferencePage',
      'ApprovalPage',
      'WorkerPage',
      'PublicationPage',
      'EnterpriseOrganizationPage',
      'EnterpriseMembershipPage',
      'EnterpriseProjectRepositoryPage',
      'EnterprisePolicyPage',
      'EnterpriseFleetPage',
      'EnterpriseUsagePage',
      'EnterpriseAuditPage',
      'EnterpriseIntegrationPage',
      'EnterpriseIdentityPage',
      'CollaborationActivityPage',
      'CollaborationNotificationPage',
      'CollaborationPresencePage',
    ].map(name => schema.$defs[name].properties.kind.const),
    [
      'product_session_page',
      'chat_interaction_page',
      'delivery_page',
      'candidate_history_page',
      'candidate_file_page',
      'model_route_availability_page',
      'credential_reference_page',
      'approval_page',
      'worker_page',
      'publication_page',
      'enterprise_organization_page',
      'enterprise_membership_page',
      'enterprise_project_repository_page',
      'enterprise_policy_page',
      'enterprise_fleet_page',
      'enterprise_usage_page',
      'enterprise_audit_page',
      'enterprise_integration_page',
      'enterprise_identity_page',
      'collaboration_activity_page',
      'collaboration_notification_page',
      'collaboration_presence_page',
    ],
  )

  const pagination = schema['x-winwincode-semantics'].pagination
  assert.equal(pagination.order, 'snapshot_then_updated_at_then_id')
  assert.equal(pagination.cursor, 'opaque_scope_query_filter_bound')
  assert.equal(pagination.invalidCursorError, 'INVALID_REQUEST')

  const candidateRead = schema['x-winwincode-semantics'].candidateReviewRead
  assert.deepEqual(candidateRead.queries, [
    'candidate.list',
    'candidate.review.get',
    'candidate.files.list',
    'candidate.diff.get',
  ])
  assert.equal(candidateRead.diffChunkMaxBytes, 262_144)
  assert.deepEqual(candidateRead.binding, [
    'repository scope',
    'deliveryId',
    'deliveryRevision',
    'readPageLimit',
    'candidateRef',
    'candidateTreeId',
    'diffSha256',
  ])
  assert.equal(candidateRead.pathTraversalAllowed, false)
  assert.equal(candidateRead.rawRepositoryLocatorAllowed, false)
  assert.equal(candidateRead.callerGitRevisionAllowed, false)
  assert.equal(candidateRead.baseProjectionIncludesContent, false)
  assert.match(candidateRead.historyAvailability, /display-only/u)
  assert.match(candidateRead.historicalReviewAuthorization, /never become current/u)

  const modelRouteAvailability = schema['x-winwincode-semantics'].modelRouteAvailability
  assert.equal(modelRouteAvailability.query, 'model.route.availability.list')
  assert.match(modelRouteAvailability.scopeAuthority, /exact repository/u)
  assert.deepEqual(modelRouteAvailability.sources, [
    'effective model settings selection',
    'effective Provider/model catalog',
    'Credential reference lifecycle',
    'configured durable model request pool',
  ])
  assert.deepEqual(schema.$defs.ModelRouteAvailabilityReason.enum, [
    'ready',
    'no_provider',
    'credential_missing_or_revoked',
    'default_route_invalid',
    'provider_or_model_disabled',
    'request_pool_unavailable',
  ])
  assert.equal(modelRouteAvailability.clientInferenceAllowed, false)
  assert.equal(modelRouteAvailability.secretFieldsAllowed, false)
  assert.equal(modelRouteAvailability.poolInternalFieldsAllowed, false)
  assert.equal(
    modelRouteAvailability.invalidationEvent,
    'model-route-availability.invalidated.v1',
  )
})

test('publication.get exposes one bounded secret-safe detail while publication.list stays compact', async () => {
  const schema = await json('control-plane-http.schema.json')
  const detail = schema.$defs.PublicationDetailProjection
  assert.equal(detail.additionalProperties, false)
  assert.equal(detail.properties.kind.const, 'publication_detail')
  assert.equal(detail.properties.summary.$ref, '#/$defs/PublicationProjection')
  assert.equal(detail.properties.steps.maxItems, 4)
  assert.equal(detail.properties.steps.items.$ref, '#/$defs/PublicationStepProjection')
  assert.equal(detail.properties.history.maxItems, 200)
  assert.equal(
    detail.properties.history.items.$ref,
    '#/$defs/PublicationStatusHistoryProjection',
  )
  assert.deepEqual(detail.properties.cancellation.oneOf, [
    { $ref: '#/$defs/PublicationCancellationProjection' },
    { type: 'null' },
  ])
  assert.ok(detail.required.includes('historyTruncated'))
  assert.ok(detail.required.includes('retryable'))
  assert.ok(detail.required.includes('cancellable'))

  const step = schema.$defs.PublicationStepProjection
  assert.deepEqual(step.properties.kind.enum, [
    'branch',
    'pull_request',
    'issue_comment',
    'commit_status',
  ])
  assert.deepEqual(step.properties.state.enum, [
    'pending',
    'applying',
    'unknown',
    'succeeded',
    'rejected',
  ])
  assert.equal(step.properties.outcomeCode.oneOf[0].pattern, '^[A-Za-z0-9][A-Za-z0-9._-]{0,99}$')
  assert.deepEqual(step.properties.resourceRef.oneOf, [
    { $ref: '#/$defs/PublicationResourceRef' },
    { type: 'null' },
  ])

  const history = schema.$defs.PublicationStatusHistoryProjection
  assert.equal(history.properties.stepStates.maxItems, 4)
  assert.equal(
    history.properties.stepStates.items.$ref,
    '#/$defs/PublicationStepStateProjection',
  )
  assert.equal(history.properties.revision.$ref, './domain.schema.json#/$defs/Revision')
  assert.equal(history.properties.updatedAt.$ref, './domain.schema.json#/$defs/Instant')

  const listItems = schema.$defs.PublicationPage.properties.items.items
  assert.equal(listItems.$ref, '#/$defs/PublicationProjection')
  const forbidden = JSON.stringify([
    detail.properties,
    step.properties,
    history.properties,
    schema.$defs.PublicationCancellationProjection.properties,
  ])
  for (const name of [
    'providerRequest',
    'providerResponse',
    'credential',
    'idempotencyKey',
    'operationKey',
    'requestSha256',
    'rawReceipt',
    'rawRequest',
    'actorDigest',
    'url',
  ]) assert.doesNotMatch(forbidden, new RegExp(name, 'iu'))
})

test('HTTP input responses are bound and cannot inject an ExecutionPort message', async () => {
  const schema = await json('control-plane-http.schema.json')
  const input = schema.$defs.InputRespondPayload

  assert.deepEqual(input.required, [
    'productSessionId',
    'workerSessionId',
    'executionJobId',
    'inputRequestId',
    'status',
    'value',
    'sessionIdentity',
  ])
  assert.equal(
    input.properties.productSessionId.$ref,
    './domain.schema.json#/$defs/ProductSessionId',
  )
  assert.equal(
    input.properties.workerSessionId.$ref,
    './domain.schema.json#/$defs/WorkerSessionId',
  )
  assert.equal(
    input.properties.executionJobId.$ref,
    './domain.schema.json#/$defs/ExecutionJobId',
  )
  assert.equal(
    input.properties.inputRequestId.$ref,
    './domain.schema.json#/$defs/InputRequestId',
  )
  assert.equal(
    input.properties.value.oneOf[0].$ref,
    './domain.schema.json#/$defs/InteractiveInputValue',
  )
  assert.deepEqual(schema['x-winwincode-semantics'].inputResponse, {
    binding: [
      'actor',
      'scope',
      'expectedRevision',
      'productSessionId',
      'workerSessionId',
      'executionJobId',
      'inputRequestId',
      'sessionIdentity',
    ],
    mapsAfterValidationTo: 'execution-port:input.response',
    rejectsArbitraryExecutionMessages: true,
    identityJoin: 'payload.sessionIdentity must equal the accepted SessionBinding identity',
  })
  assert.equal(JSON.stringify(input).includes('messageId'), false)
  assert.equal(JSON.stringify(input).includes('lease'), false)
  assert.equal(JSON.stringify(input).includes('kind'), false)
})

test('Delivery Spec input carries editable scope and one explicit ProductSession source', async () => {
  const schema = await json('control-plane-http.schema.json')
  const input = schema.$defs.DeliverySpecInput
  for (const field of ['scope', 'outOfScope', 'constraints', 'sourceProductSessionId']) {
    assert.ok(input.required.includes(field), `${field} is canonical input`)
  }
  assert.deepEqual(input.properties.scope, {
    type: 'array',
    items: { type: 'string', minLength: 1, maxLength: 65536 },
    minItems: 1,
    maxItems: 1000,
    uniqueItems: true,
  })
  for (const field of ['outOfScope', 'constraints']) {
    assert.deepEqual(input.properties[field], {
      type: 'array',
      items: { type: 'string', minLength: 1, maxLength: 65536 },
      maxItems: 1000,
      uniqueItems: true,
    })
  }
  assert.deepEqual(input.properties.sourceProductSessionId.oneOf, [
    { $ref: './domain.schema.json#/$defs/ProductSessionId' },
    { type: 'null' },
  ])

  const output = schema.$defs.DeliveryRequirementsProjection
  assert.ok(output.required.includes('sourceProductSessionId'))
  assert.deepEqual(output.properties.sourceProductSessionId, {
    oneOf: [
      { $ref: './domain.schema.json#/$defs/ProductSessionId' },
      { type: 'null' },
    ],
  })
})

test('enterprise identity contract returns metadata without API Token material', async () => {
  const schema = await json('control-plane-http.schema.json')
  const semantics = schema['x-winwincode-semantics'].enterpriseIdentity
  const issue = schema.$defs.EnterpriseApiTokenIssuePayload
  const projection = schema.$defs.EnterpriseApiTokenProjection

  assert.deepEqual(semantics, {
    authority: 'one_durable_identity_ledger',
    externalIdentityActor: 'external_subject_maps_to_exact_user_actor',
    tokenFormat:
      'wwc_api_<26-character ApiTokenId suffix>.<43-character unpadded base64url encoding of exactly 32 random bytes>',
    tokenPersistence: 'sha256_verifier_only',
    secretSubmission:
      'raw_token_generated_and_retained_by_caller; only verifier enters command',
    replayIdentity: 'actor+organization_scope+requestId',
    rotation: 'expectedRevision_and_exact_request_replay',
    revocation: 'checked_on_every_HTTP_and_WebSocket_authentication',
    crossTenant: 'every_authorized_scope_must_share_the_command_organization',
    audit: 'identity_lifecycle_mutations_are_durable_and_secret_free',
  })
  assert.ok(issue.required.includes('tokenSha256'))
  assert.equal(
    issue.properties.tokenSha256.$ref,
    './domain.schema.json#/$defs/Sha256Digest',
  )
  assert.equal(projection.properties.tokenSha256, undefined)
  assert.equal(projection.properties.rawToken, undefined)
  assert.equal(projection.properties.secret, undefined)
  assert.equal(schema.$defs.EnterpriseIdentityPage.properties.items.maxItems, 100)
})

test('Chat interaction snapshots expose one complete secret-safe binding contract', async () => {
  const schema = await json('control-plane-http.schema.json')
  assert.deepEqual(schema.$defs.ChatInteractionBindingProjection.required, [
    'productSessionId',
    'executionJobId',
    'workerSessionId',
    'sessionIdentity',
  ])
  assert.deepEqual(schema.$defs.ApprovalProjection.required, [
    'id',
    'revision',
    'state',
    'requestedAt',
    'expiresAt',
    'subject',
    'category',
    'effectiveDecisionScope',
    'sanitizedDetail',
    'binding',
  ])
  assert.deepEqual(schema.$defs.ApprovalProjectionCategory.enum, [
    'filesystem_write',
    'mcp',
    'network',
    'shell',
    'unavailable',
  ])
  assert.deepEqual(schema.$defs.ApprovalEffectiveDecisionScope.enum, ['once'])
  assert.deepEqual(schema.$defs.ApprovalSanitizedDetailUnavailableReason.enum, [
    'producer_unavailable',
    'encoded_payload_redacted',
    'source_not_recorded',
  ])
  assert.deepEqual(schema.$defs.ApprovalSanitizedDetailProjection.required, [
    'kind',
    'reason',
  ])
  assert.equal(schema.$defs.ApprovalSanitizedDetailProjection.properties.kind.const, 'unavailable')
  assert.equal(schema.$defs.ApprovalSanitizedDetailProjection.properties.command, undefined)
  assert.equal(schema.$defs.ApprovalSanitizedDetailProjection.properties.cwd, undefined)
  assert.equal(schema.$defs.ApprovalSanitizedDetailProjection.properties.files, undefined)
  assert.equal(schema.$defs.ApprovalSanitizedDetailProjection.properties.network, undefined)
  assert.equal(schema.$defs.ApprovalSanitizedDetailProjection.properties.mcp, undefined)
  assert.equal(schema.$defs.ApprovalSanitizedDetailProjection.properties.risk, undefined)
  assert.equal(schema.$defs.ApprovalSanitizedDetailProjection.properties.reasonText, undefined)
  assert.ok(schema.$defs.ApprovalDecidePayload.required.includes('binding'))
  assert.equal(schema.$defs.ChatInputInteractionProjection.properties.details, undefined)
  assert.equal(schema.$defs.ChatInputInteractionProjection.properties.payload, undefined)
  assert.equal(schema.$defs.ChatInputInteractionProjection.properties.credential, undefined)
  assert.deepEqual(
    schema.$defs.ChatInteractionProjection.oneOf.map(branch => branch.$ref),
    [
      '#/$defs/ChatInputInteractionProjection',
      '#/$defs/ChatApprovalInteractionProjection',
    ],
  )
})

test('HTTP command retry, revision conflict, and errors have stable machine semantics', async () => {
  const schema = await json('control-plane-http.schema.json')
  const semantics = schema['x-winwincode-semantics']

  assert.deepEqual(semantics.writeTransport, {
    accepted: 'HTTP_POST_COMMAND',
    rejected: ['HTTP_WEBSOCKET_COMMAND', 'HTTP_ROUTE_ALIAS'],
  })
  assert.deepEqual(semantics.idempotency, {
    identity: 'actor+scope+requestId',
    sameRequest: 'return_original_status_and_body_without_reexecution',
    changedRequest: 'IDEMPOTENCY_CONFLICT',
  })
  assert.deepEqual(semantics.revision, {
    checkedBeforeMutation: true,
    conflict: 'REVISION_CONFLICT',
    reports: ['expectedRevision', 'currentRevision'],
  })
  assert.deepEqual(
    specializationNames(schema, 'CommandCompletedResponse', 'command'),
    COMMANDS,
  )
  assert.equal(schema.$defs.CommandResult, undefined)
  assert.deepEqual(semantics.responseCorrelation, {
    authority: 'request_operation_discriminator',
    queryResult: 'exact_query_to_result_projection',
    commandResult: 'exact_command_to_result_projection',
    relabeling: 'reject',
  })

  assert.deepEqual(
    Object.fromEntries(Object.entries(semantics.errors).map(([code, value]) => [
      code,
      value.httpStatus,
    ])),
    ERROR_STATUS,
  )
  assert.equal(semantics.errors.RATE_LIMITED.retryable, true)
  assert.equal(semantics.errors.READ_CURSOR_EXPIRED.retryable, true)
  assert.equal(semantics.errors.SERVICE_UNAVAILABLE.retryable, true)
  assert.equal(semantics.errors.TRUSTED_FACTS_UNAVAILABLE.retryable, true)
  for (const code of Object.keys(ERROR_STATUS).filter(code => (
    code !== 'RATE_LIMITED'
    && code !== 'READ_CURSOR_EXPIRED'
    && code !== 'SERVICE_UNAVAILABLE'
    && code !== 'TRUSTED_FACTS_UNAVAILABLE'
  ))) assert.equal(semantics.errors[code].retryable, false)
})

test('OpenAPI 3.1 exposes one browser-session route and the canonical business routes', async () => {
  const schema = await json('control-plane-http.schema.json')
  const generatedOpenApi = await json('openapi.generated.json')
  const openapi = schema['x-winwincode-openapi']

  assert.deepEqual(Object.keys(openapi), ['securitySchemes', 'paths'])
  assert.deepEqual(openapi.securitySchemes, {
    sessionCookie: {
      type: 'apiKey',
      in: 'cookie',
      name: 'wwc_session',
      description: 'Same-origin Web and local Host session.',
    },
    bearerAuth: {
      type: 'http',
      scheme: 'bearer',
      bearerFormat: 'JWT',
      description: 'Service-account and enterprise access token.',
    },
    bootstrapProof: {
      type: 'http',
      scheme: 'bearer',
      bearerFormat: 'opaque',
      description: 'Short-window, write-only browser bootstrap proof accepted only by session creation.',
    },
  })
  assert.deepEqual(generatedOpenApi.components.securitySchemes, openapi.securitySchemes)
  assert.deepEqual(Object.keys(openapi.paths), [
    '/api/v1/auth/session',
    '/api/v1/commands',
    '/api/v1/queries',
  ])
  const authSession = openapi.paths['/api/v1/auth/session']
  assert.deepEqual(Object.keys(authSession), ['get', 'post', 'delete'])
  assert.deepEqual(authSession.get.security, [{ sessionCookie: [] }])
  assert.deepEqual(authSession.post.security, [{ bootstrapProof: [] }])
  assert.deepEqual(authSession.delete.security, [{ sessionCookie: [] }])
  assert.equal(
    authSession.get.responses['200'].content['application/json'].schema.$ref,
    './control-plane-http.schema.json#/$defs/AuthSessionResponse',
  )
  assert.equal(
    authSession.post.responses['201'].content['application/json'].schema.$ref,
    './control-plane-http.schema.json#/$defs/AuthSessionResponse',
  )
  assert.equal(
    openapi.paths['/api/v1/commands'].post.requestBody.content['application/json'].schema.$ref,
    './control-plane-http.schema.json#/$defs/CommandRequest',
  )
  assert.equal(
    openapi.paths['/api/v1/queries'].post.requestBody.content['application/json'].schema.$ref,
    './control-plane-http.schema.json#/$defs/QueryRequest',
  )

  const commandResponses = openapi.paths['/api/v1/commands'].post.responses
  const queryResponses = openapi.paths['/api/v1/queries'].post.responses
  assert.deepEqual(Object.keys(commandResponses), [
    '200',
    '202',
    '400',
    '401',
    '403',
    '404',
    '409',
    '429',
    '500',
    '503',
  ])
  assert.deepEqual(Object.keys(queryResponses), [
    '200',
    '400',
    '401',
    '403',
    '404',
    '409',
    '429',
    '500',
    '503',
  ])
  for (const route of [
    openapi.paths['/api/v1/commands'],
    openapi.paths['/api/v1/queries'],
  ]) {
    assert.deepEqual(route.post.security, [
      { sessionCookie: [] },
      { bearerAuth: [] },
    ])
    for (const response of Object.values(route.post.responses)) {
      if (response.content === undefined) continue
      assert.ok(response.content['application/json'])
      assert.equal(response.content['application/problem+json'], undefined)
    }
  }
  for (const route of [
    generatedOpenApi.paths['/api/v1/commands'],
    generatedOpenApi.paths['/api/v1/queries'],
  ]) {
    assert.deepEqual(route.post.security, [
      { sessionCookie: [] },
      { bearerAuth: [] },
    ])
  }

  assert.deepEqual(schema['x-winwincode-semantics'].authentication, {
    anonymousAllowed: false,
    acceptedMethods: ['sessionCookie', 'bearerAuth'],
    principalBinding: 'authenticated_principal_must_equal_envelope_actor',
    credentialsInQueryAllowed: false,
    credentialsInBodyAllowed: false,
  })
  assert.deepEqual(schema['x-winwincode-semantics'].browserSessionContext, {
    source: 'authenticated_cookie_session_only',
    bootstrapResponse: 'AuthSessionResponse',
    reloadResponse: 'AuthSessionResponse',
    actorBinding: 'response_actor_equals_authenticated_principal',
    scopeBinding: 'response_scopes_are_the_current_bounded_authorization_set',
    scopeOrder: 'canonical_scope_identity',
    scopeShrink: 'subsequent_context_reads_return_only_current_scopes',
    scopeRevocation: 'empty_authorization_revokes_the_session',
    clientReadableCredentials: false,
  })
  assert.deepEqual(schema.$defs.AuthSessionResponse.required, [
    'schemaVersion',
    'expiresAt',
    'actor',
    'authorizedScopes',
  ])
  assert.equal(schema.$defs.AuthSessionResponse.additionalProperties, false)
  assert.equal(schema.$defs.AuthSessionResponse.properties.authorizedScopes.minItems, 1)
  assert.equal(schema.$defs.AuthSessionResponse.properties.authorizedScopes.maxItems, 100)

  const taskApproval = schema.$defs.DeliveryApproveTaskBreakdownPayload
  assert.deepEqual(taskApproval.required, ['deliveryId', 'reviewSetSha256'])
  assert.equal(taskApproval.properties.tasks, undefined)
  assert.deepEqual(schema['x-winwincode-semantics'].taskBreakdownApproval, {
    payload: ['deliveryId', 'reviewSetSha256'],
    authority: 'current_sealed_approved_solution_review',
    callerTaskFieldsAllowed: false,
    promotion: 'copy_ordered_task_proposals_field_by_field',
    staleDigestError: 'REVISION_CONFLICT',
  })
})

test('positive and negative samples pin retries, conflicts, cursors, and secret-safe output', async () => {
  const examples = await json('examples', 'control-plane-http.examples.json')

  assert.equal(examples.schemaVersion, 1)
  assert.deepEqual(examples.idempotency.retry, examples.idempotency.original)
  assert.equal(
    examples.idempotency.conflict.requestId,
    examples.idempotency.original.requestId,
  )
  assert.notDeepEqual(examples.idempotency.conflict, examples.idempotency.original)
  assert.equal(examples.idempotency.expectedConflict.error.code, 'IDEMPOTENCY_CONFLICT')
  assert.equal(examples.revisionConflict.error.code, 'REVISION_CONFLICT')
  assert.deepEqual(examples.revisionConflict.error.details, {
    expectedRevision: 18,
    currentRevision: 19,
  })
  assert.equal(examples.invalidCursor.error.code, 'INVALID_REQUEST')
  assert.equal(examples.invalidCursor.error.details.field, 'page.cursor')
  assert.equal(examples.readCursorExpired.error.code, 'READ_CURSOR_EXPIRED')
  assert.equal(examples.readCursorExpired.error.retryable, true)
  assert.deepEqual(examples.readCursorExpired.error.details, {
    field: 'atCursor',
    action: 'restart_strongflow_read_without_cursor',
  })
  assert.equal(examples.responses.queryPage.result.kind, 'delivery_page')
  assert.deepEqual(examples.responses.commandCompleted.result, {
    id: 'wrk_00000000000000000000000000',
    revision: 19,
    state: 'draining',
    capacity: 4,
    lastHeartbeatAt: '2026-08-24T10:00:00.000Z',
  })

  const serialized = JSON.stringify(examples.responses)
  for (const forbidden of ['secretMaterial', 'accessToken', 'apiKey']) {
    assert.equal(serialized.includes(forbidden), false)
  }
  assert.equal(examples.responses.credentialReference.secretState, 'available')
  assert.equal(examples.positive.inputRespond.command, 'input.respond')
  assert.equal(examples.positive.inputRespond.payload.status, 'provided')
  assert.equal(examples.positive.sessionMessagesList.query, 'session.messages.list')
  assert.equal(examples.positive.runtimeProjectionGet.query, 'runtime.projection.get')
  assert.deepEqual(examples.positive.deliveryApproveTaskBreakdown.payload, {
    deliveryId: 'dlv_00000000000000000000000000',
    reviewSetSha256:
      'sha256:0000000000000000000000000000000000000000000000000000000000000000',
  })
  assert.equal(examples.responses.chatMessagesPage.result.kind, 'chat_message_page')
  assert.equal(examples.responses.publicationDetail.result.kind, 'publication_detail')
  assert.equal(examples.responses.publicationDetail.result.historyTruncated, true)
  assert.equal(examples.responses.publicationDetail.result.history.at(-1).revision, 201)
  assert.equal(
    examples.responses.deliveryDetailPendingReview.result.solutionReview.reviewStatus,
    'pending',
  )
  assert.equal(
    examples.responses.deliveryDetailPendingReview.result.solutionReview.taskProposals.length,
    1,
  )
  assert.equal(examples.responses.runtimeProjection.result.kind, 'runtime_projection')
  assert.equal(examples.responses.runtimeProjection.result.sessions[0].asOfSequence, 42)
  assert.deepEqual(
    examples.positive.runtimeProjectionGet.parameters.atCursor,
    examples.responses.runtimeProjection.result.readCursor,
  )
  assert.notDeepEqual(
    examples.strongFlowReadCutMismatch.deliveryCursor,
    examples.strongFlowReadCutMismatch.runtimeCursor,
  )
  assert.equal(
    examples.strongFlowReadCutMismatch.expected.error.code,
    'REVISION_CONFLICT',
  )
})
