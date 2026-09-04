import assert from 'node:assert/strict'
import test from 'node:test'

import {
  CLIENT_CONTROL_COMMAND_MESSAGE_KINDS,
  CLIENT_CONTROL_MESSAGE_KINDS,
  CLIENT_CONTROL_OCCUPANCY_FENCED_MESSAGE_KINDS,
  CLIENT_CONTROL_SCHEMA_VERSION,
  CLIENT_TO_SERVER_MESSAGE_KINDS,
  CLIENT_TO_SERVER_OCCUPANCY_FENCED_MESSAGE_KINDS,
  SERVER_TO_CLIENT_MESSAGE_KINDS,
  SERVER_TO_CLIENT_OCCUPANCY_FENCED_MESSAGE_KINDS,
  parseClientCapacityReport,
  parseClientControlError,
  parseClientControlMessage,
  parseClientOccupancyLease,
  parseClientToServerMessage,
  parseClientConnectCode,
  parseRepositoryBinding,
  parseRepositoryBindingProjection,
  parseServerToClientMessage,
  parseWorkerLaunchGrant,
} from '../packages/contracts/dist/index.js'

// Schema-alignment contract for the ClientControlPort TypeScript projection.
// Authoritative source: schema/winwincode/v1/client-control.schema.json (with
// its domain.schema.json sibling). Messages are flat: the schema inlines the
// envelope and command fields into every message, so expectedRevision,
// idempotencyKey, occupancyLeaseId, and occupancyFencingToken are siblings of
// `kind`, never members of a payload sub-object.

const T0 = '2026-09-01T09:30:15.000Z'
const T1 = '2026-09-01T09:41:18.000Z'
const DIGEST = `sha256:${'a'.repeat(64)}`
const COMMIT = 'b'.repeat(40)
const CANDIDATE_REF = 'refs/winwincode/candidates/demo-1'

function crockfordId(prefix, serial = 1) {
  return `${prefix}_${String(serial).padStart(26, '0')}`
}

let messageSerial = 0

function envelopeFields(kind) {
  messageSerial += 1
  return {
    schemaVersion: CLIENT_CONTROL_SCHEMA_VERSION,
    messageId: crockfordId('cmsg', messageSerial),
    clientNodeId: crockfordId('cnd'),
    clientInstanceId: crockfordId('cix'),
    sequence: 700 + messageSerial,
    occurredAt: T0,
    kind,
  }
}

function capacityReport() {
  return {
    maxConcurrentWorkerSessions: 8,
    runningWorkerSessions: 1,
    reservedWorkerSessions: 0,
    drainingWorkerSessions: 0,
  }
}

function bindingProjection() {
  return {
    repositoryBindingId: crockfordId('rbd'),
    displayName: 'WinWinCode',
    repositoryKind: 'git',
    defaultBranch: 'main',
    headCommit: COMMIT,
    dirtyState: 'clean',
    availability: 'available',
    repositoryFingerprint: DIGEST,
    lastScannedAt: T0,
  }
}

function localCandidateReceipt() {
  return {
    localCandidateReceiptId: crockfordId('lcr'),
    candidateRef: CANDIDATE_REF,
    repositoryBindingId: crockfordId('rbd'),
    candidateCommit: COMMIT,
    localRefName: 'refs/heads/winwincode/candidates/demo-1',
    state: 'retained',
    createdAt: T0,
    revision: 1,
  }
}

function localApplyReceipt() {
  return {
    localApplyReceiptId: crockfordId('lar'),
    candidateRef: CANDIDATE_REF,
    repositoryBindingId: crockfordId('rbd'),
    targetBranch: 'main',
    expectedHead: COMMIT,
    strategy: 'create_branch',
    result: 'applied',
    resultingCommit: COMMIT,
    conflictArtifactRef: null,
    createdAt: T0,
    revision: 1,
  }
}

function workerLaunchGrant() {
  return {
    workerLaunchGrantId: crockfordId('wlg'),
    clientNodeId: crockfordId('cnd'),
    clientInstanceId: crockfordId('cix'),
    occupancyLeaseId: crockfordId('ocl'),
    occupancyFencingToken: '3',
    repositoryBindingId: crockfordId('rbd'),
    productSessionId: crockfordId('psn'),
    stageRunId: crockfordId('run'),
    workerSessionId: crockfordId('wsn'),
    workerId: crockfordId('wrk'),
    workerInstanceId: crockfordId('wki'),
    credentialDigest: DIGEST,
    expiresAt: T1,
    state: 'issued',
    revision: 1,
  }
}

// Minimal schema-valid extra fields for every kind; the command base and the
// fencing pair are added by the builders below according to the schema.
const KIND_EXTRA_FIELDS = Object.freeze({
  'client.enroll': () => ({
    displayName: 'Wen-ge MacBook Pro',
    platform: 'aarch64-apple-darwin',
    architecture: 'aarch64',
    clientVersion: '0.1.0',
  }),
  'client.hello': () => ({
    clientVersion: '0.1.0',
    presenceState: 'online',
    acceptingConnections: true,
    lockState: 'unlocked',
    capacity: capacityReport(),
  }),
  'client.heartbeat': () => ({
    presenceState: 'online',
    acceptingConnections: true,
    lockState: 'unlocked',
    capacity: capacityReport(),
    occupancyLeaseId: null,
  }),
  'client.connect_code.published': () => ({
    connectCodeId: crockfordId('cct'),
    codeDigest: DIGEST,
    expiresAt: T1,
  }),
  'client.access.challenge_ack': () => ({
    challengeId: crockfordId('cac'),
    connectCodeId: crockfordId('cct'),
    status: 'confirmed',
  }),
  'client.occupancy.ack': () => ({}),
  'client.occupancy.rejected': () => ({ reason: 'stale_fencing_token' }),
  'client.repository.upsert': () => ({ repository: bindingProjection() }),
  'client.repository.removed': () => ({ repositoryBindingId: crockfordId('rbd') }),
  'client.repository.status': () => ({
    repositoryBindingId: crockfordId('rbd'),
    availability: 'available',
    dirtyState: 'clean',
    headCommit: COMMIT,
    lastScannedAt: T0,
  }),
  'client.worker.launch_ack': () => ({
    workerLaunchGrantId: crockfordId('wlg'),
    workerSessionId: crockfordId('wsn'),
    workerId: crockfordId('wrk'),
    workerInstanceId: crockfordId('wki'),
    status: 'accepted',
  }),
  'client.worker.state': () => ({
    workerSessionId: crockfordId('wsn'),
    workerInstanceId: crockfordId('wki'),
    occupancyLeaseId: null,
    state: 'running',
    observedAt: T0,
    exitCode: null,
  }),
  'client.worker.reconcile': () => ({
    occupancyLeaseId: null,
    workers: [{
      workerSessionId: crockfordId('wsn'),
      workerInstanceId: crockfordId('wki'),
      reconcileState: 'still_running',
      observedAt: T0,
    }],
  }),
  'client.candidate.retained': () => ({
    workerSessionId: crockfordId('wsn'),
    receipt: localCandidateReceipt(),
  }),
  'client.candidate.apply_result': () => ({ receipt: localApplyReceipt() }),
  'client.command_ack': () => ({
    commandMessageId: crockfordId('cmsg', 999),
    commandKind: 'client.enroll',
    status: 'accepted',
  }),
  'client.enrollment_accepted': () => ({
    publicClientId: '927351842',
    serverTime: T0,
    heartbeatIntervalMs: 30_000,
  }),
  'client.access.challenge': () => ({
    challengeId: crockfordId('cac'),
    connectCodeId: crockfordId('cct'),
    codeDigest: DIGEST,
    requesterUserId: crockfordId('usr'),
    expiresAt: T1,
  }),
  'client.occupancy.offer': () => ({
    holderUserId: crockfordId('usr'),
    claimRequestId: crockfordId('ocq'),
    claimedAt: T0,
    idleExpiresAt: null,
  }),
  'client.occupancy.release': () => ({ mode: 'immediate' }),
  'client.occupancy.force_fence': () => ({
    supersededLeaseId: null,
    reason: 'recovery_deadline_exceeded',
  }),
  'client.repository.rescan': () => ({
    repositoryBindingId: crockfordId('rbd'),
    reason: 'policy',
  }),
  'client.worker.launch': () => ({ launchGrant: workerLaunchGrant() }),
  'client.worker.stop': () => ({
    workerSessionId: crockfordId('wsn'),
    workerId: crockfordId('wrk'),
    reason: 'occupant_requested',
  }),
  'client.candidate.apply': () => ({
    repositoryBindingId: crockfordId('rbd'),
    candidateRef: CANDIDATE_REF,
    targetBranch: 'main',
    expectedHead: COMMIT,
    strategy: 'create_branch',
    requesterUserId: crockfordId('usr'),
  }),
  'client.client_lock': () => ({ lockState: 'locked' }),
  'client.credential_rotate': () => ({ reason: 'scheduled' }),
})

// The 8 non-command messages (reports, ack, response, request) and the 8
// unfenced commands; the 11 fenced commands get their pair from the builders.
const NON_COMMAND_KINDS = Object.freeze([
  'client.hello',
  'client.heartbeat',
  'client.worker.state',
  'client.worker.reconcile',
  'client.repository.status',
  'client.command_ack',
  'client.enrollment_accepted',
  'client.access.challenge',
])

function validMessage(kind, overrides = {}) {
  const extra = KIND_EXTRA_FIELDS[kind]()
  const command = CLIENT_CONTROL_COMMAND_MESSAGE_KINDS.includes(kind)
    ? { expectedRevision: 4, idempotencyKey: `idem-${kind.replaceAll('.', '-')}` }
    : {}
  const fenced = CLIENT_CONTROL_OCCUPANCY_FENCED_MESSAGE_KINDS.includes(kind)
    ? { occupancyLeaseId: crockfordId('ocl'), occupancyFencingToken: '3' }
    : {}
  return {
    ...envelopeFields(kind),
    ...fenced,
    ...extra,
    ...command,
    ...overrides,
  }
}

function parseByDirection(message) {
  return CLIENT_TO_SERVER_MESSAGE_KINDS.includes(message.kind)
    ? parseClientToServerMessage(message)
    : parseServerToClientMessage(message)
}

function errorCodeOf(block) {
  try {
    block()
  } catch (error) {
    return error.code
  }
  return null
}

test('schemaVersion is the string constant winwincode/v1', () => {
  assert.equal(CLIENT_CONTROL_SCHEMA_VERSION, 'winwincode/v1')
  assert.equal(typeof CLIENT_CONTROL_SCHEMA_VERSION, 'string')
})

test('kind registries match the schema verbatim: 16 + 11 = 27', () => {
  assert.equal(CLIENT_TO_SERVER_MESSAGE_KINDS.length, 16)
  assert.equal(SERVER_TO_CLIENT_MESSAGE_KINDS.length, 11)
  assert.equal(CLIENT_CONTROL_MESSAGE_KINDS.length, 27)
  assert.equal(Object.isFrozen(CLIENT_TO_SERVER_MESSAGE_KINDS), true)
  assert.equal(Object.isFrozen(SERVER_TO_CLIENT_MESSAGE_KINDS), true)
  assert.equal(Object.isFrozen(CLIENT_CONTROL_COMMAND_MESSAGE_KINDS), true)
  assert.equal(Object.isFrozen(CLIENT_CONTROL_OCCUPANCY_FENCED_MESSAGE_KINDS), true)
  const overlap = CLIENT_TO_SERVER_MESSAGE_KINDS.filter(kind => (
    SERVER_TO_CLIENT_MESSAGE_KINDS.includes(kind)
  ))
  assert.deepEqual(overlap, [])
})

test('exactly 19 command kinds and 11 fenced kinds per the schema', () => {
  assert.equal(CLIENT_CONTROL_COMMAND_MESSAGE_KINDS.length, 19)
  assert.equal(CLIENT_CONTROL_OCCUPANCY_FENCED_MESSAGE_KINDS.length, 11)
  assert.deepEqual(
    [...CLIENT_CONTROL_OCCUPANCY_FENCED_MESSAGE_KINDS],
    [
      'client.occupancy.ack',
      'client.occupancy.rejected',
      'client.worker.launch_ack',
      'client.candidate.retained',
      'client.candidate.apply_result',
      'client.occupancy.offer',
      'client.occupancy.release',
      'client.occupancy.force_fence',
      'client.worker.launch',
      'client.worker.stop',
      'client.candidate.apply',
    ],
  )
  for (const kind of CLIENT_TO_SERVER_OCCUPANCY_FENCED_MESSAGE_KINDS) {
    assert.equal(CLIENT_TO_SERVER_MESSAGE_KINDS.includes(kind), true)
  }
  for (const kind of SERVER_TO_CLIENT_OCCUPANCY_FENCED_MESSAGE_KINDS) {
    assert.equal(SERVER_TO_CLIENT_MESSAGE_KINDS.includes(kind), true)
  }
})

test('every kind parses a schema-valid flat message and freezes the result', () => {
  for (const kind of CLIENT_CONTROL_MESSAGE_KINDS) {
    const message = validMessage(kind)
    const parsed = parseByDirection(message)
    assert.equal(parsed.kind, kind, kind)
    assert.equal(parsed.schemaVersion, 'winwincode/v1', kind)
    assert.equal(parsed.occurredAt, T0, kind)
    assert.equal(Object.isFrozen(parsed), true, kind)
    const unionParsed = parseClientControlMessage(message)
    assert.equal(unionParsed.kind, kind, kind)
  }
})

test('message fields live flat on the envelope, not in a payload object', () => {
  const parsed = parseClientToServerMessage(validMessage('client.heartbeat'))
  assert.equal('payload' in parsed, false)
  assert.equal(parsed.presenceState, 'online')
  assert.deepEqual(parsed.capacity, capacityReport())
})

test('every command kind requires expectedRevision and idempotencyKey', () => {
  for (const kind of CLIENT_CONTROL_COMMAND_MESSAGE_KINDS) {
    const missingRevision = validMessage(kind)
    delete missingRevision.expectedRevision
    assert.equal(
      errorCodeOf(() => parseByDirection(missingRevision)),
      'INVALID_SHAPE',
      `${kind} must require expectedRevision`,
    )
    const missingKey = validMessage(kind)
    delete missingKey.idempotencyKey
    assert.equal(
      errorCodeOf(() => parseByDirection(missingKey)),
      'INVALID_SHAPE',
      `${kind} must require idempotencyKey`,
    )
  }
})

test('non-command kinds reject the command base at runtime', () => {
  for (const kind of NON_COMMAND_KINDS) {
    assert.equal(CLIENT_CONTROL_COMMAND_MESSAGE_KINDS.includes(kind), false, kind)
    const stamped = validMessage(kind, {
      expectedRevision: 4,
      idempotencyKey: `idem-${kind.replaceAll('.', '-')}`,
    })
    assert.equal(
      errorCodeOf(() => parseByDirection(stamped)),
      'INVALID_SHAPE',
      `${kind} must reject expectedRevision and idempotencyKey`,
    )
  }
})

test('fenced commands require the lease identity pair as decimal strings', () => {
  for (const kind of CLIENT_CONTROL_OCCUPANCY_FENCED_MESSAGE_KINDS) {
    const missingLease = validMessage(kind)
    delete missingLease.occupancyLeaseId
    assert.equal(
      errorCodeOf(() => parseByDirection(missingLease)),
      'INVALID_SHAPE',
      `${kind} must require occupancyLeaseId`,
    )
    const missingToken = validMessage(kind)
    delete missingToken.occupancyFencingToken
    assert.equal(
      errorCodeOf(() => parseByDirection(missingToken)),
      'INVALID_SHAPE',
      `${kind} must require occupancyFencingToken`,
    )
    const numericToken = validMessage(kind, { occupancyFencingToken: 3 })
    assert.equal(
      errorCodeOf(() => parseByDirection(numericToken)),
      'INVALID_IDENTIFIER',
      `${kind} must reject a numeric occupancyFencingToken`,
    )
    const zeroPaddedToken = validMessage(kind, { occupancyFencingToken: '03' })
    assert.equal(
      errorCodeOf(() => parseByDirection(zeroPaddedToken)),
      'INVALID_IDENTIFIER',
      `${kind} must reject a zero-padded token`,
    )
    const zeroToken = validMessage(kind, { occupancyFencingToken: '0' })
    assert.equal(
      errorCodeOf(() => parseByDirection(zeroToken)),
      'INVALID_IDENTIFIER',
      `${kind} must reject the token zero`,
    )
    const parsed = parseByDirection(validMessage(kind))
    assert.equal(parsed.occupancyFencingToken, '3', kind)
    assert.equal(typeof parsed.occupancyFencingToken, 'string', kind)
  }
})

test('unfenced kinds reject the occupancy fencing stamp', () => {
  for (const kind of CLIENT_CONTROL_MESSAGE_KINDS) {
    if (CLIENT_CONTROL_OCCUPANCY_FENCED_MESSAGE_KINDS.includes(kind)) continue
    const stamped = validMessage(kind)
    stamped.occupancyLeaseId = crockfordId('ocl')
    stamped.occupancyFencingToken = '3'
    assert.equal(
      errorCodeOf(() => parseByDirection(stamped)),
      'INVALID_SHAPE',
      `${kind} must reject the fencing stamp`,
    )
  }
})

test('timestamps are RFC3339 Instant strings, never epoch millis', () => {
  const epochMillis = validMessage('client.heartbeat', { occurredAt: 1_800_000_000_000 })
  assert.equal(
    errorCodeOf(() => parseClientToServerMessage(epochMillis)),
    'INVALID_VALUE',
  )
  const withoutMillis = validMessage('client.heartbeat', { occurredAt: '2026-09-01T09:30:15Z' })
  assert.equal(
    errorCodeOf(() => parseClientToServerMessage(withoutMillis)),
    'INVALID_VALUE',
  )
})

test('numeric schemaVersion is rejected by the string-constant gate', () => {
  const numeric = validMessage('client.heartbeat', { schemaVersion: 1 })
  assert.equal(
    errorCodeOf(() => parseClientToServerMessage(numeric)),
    'UNSUPPORTED_SCHEMA_VERSION',
  )
  const otherString = validMessage('client.heartbeat', { schemaVersion: 'winwincode/v2' })
  assert.equal(
    errorCodeOf(() => parseClientToServerMessage(otherString)),
    'UNSUPPORTED_SCHEMA_VERSION',
  )
})

test('wrong-direction and unknown kinds are rejected', () => {
  const offer = validMessage('client.occupancy.offer')
  assert.equal(errorCodeOf(() => parseClientToServerMessage(offer)), 'INVALID_VALUE')
  const enroll = validMessage('client.enroll')
  assert.equal(errorCodeOf(() => parseServerToClientMessage(enroll)), 'INVALID_VALUE')
  const unknown = validMessage('client.heartbeat', { kind: 'client.something_else' })
  assert.equal(errorCodeOf(() => parseClientToServerMessage(unknown)), 'INVALID_VALUE')
  const missingKind = validMessage('client.heartbeat')
  delete missingKind.kind
  assert.equal(errorCodeOf(() => parseClientToServerMessage(missingKind)), 'INVALID_SHAPE')
})

test('worker.state and worker.reconcile are reports: nullable lease, no fencing token', () => {
  const state = parseClientToServerMessage(validMessage('client.worker.state'))
  assert.equal(state.occupancyLeaseId, null)
  assert.equal(state.exitCode, null)
  assert.equal('expectedRevision' in state, false)
  assert.equal('occupancyFencingToken' in state, false)
  const leasedState = validMessage('client.worker.state', { occupancyLeaseId: crockfordId('ocl', 2) })
  assert.equal(parseClientToServerMessage(leasedState).occupancyLeaseId, crockfordId('ocl', 2))
  const reconcile = parseClientToServerMessage(validMessage('client.worker.reconcile'))
  assert.equal(reconcile.workers.length, 1)
  assert.equal(reconcile.occupancyLeaseId, null)
})

test('repository upsert carries the path-free binding projection', () => {
  const parsed = parseServerToClientMessage(validMessage('client.repository.rescan'))
  assert.equal(parsed.reason, 'policy')
  const projection = parseRepositoryBindingProjection(bindingProjection())
  assert.equal(Object.isFrozen(projection), true)
  assert.equal('clientNodeId' in projection, false)
  assert.equal('revision' in projection, false)
  const withAbsolutePath = {
    ...bindingProjection(),
    absolutePath: 'local/repository/sample',
  }
  assert.equal(
    errorCodeOf(() => parseRepositoryBindingProjection(withAbsolutePath)),
    'INVALID_SHAPE',
  )
})

test('worker launch binds the grant to the command lease and token', () => {
  const parsed = parseServerToClientMessage(validMessage('client.worker.launch'))
  assert.equal(parsed.launchGrant.occupancyLeaseId, parsed.occupancyLeaseId)
  assert.equal(parsed.launchGrant.occupancyFencingToken, '3')
  const mismatched = validMessage('client.worker.launch')
  mismatched.occupancyLeaseId = crockfordId('ocl', 7)
  assert.equal(
    errorCodeOf(() => parseServerToClientMessage(mismatched)),
    'RELATIONSHIP_MISMATCH',
  )
  const numericGrantToken = { ...workerLaunchGrant(), occupancyFencingToken: 3 }
  assert.equal(
    errorCodeOf(() => parseWorkerLaunchGrant(numericGrantToken)),
    'INVALID_IDENTIFIER',
  )
})

test('connect code carries only the sha256 digest, never plaintext', () => {
  const connectCode = {
    connectCodeId: crockfordId('cct'),
    clientNodeId: crockfordId('cnd'),
    codeDigest: DIGEST,
    issuedByInstanceId: crockfordId('cix'),
    expiresAt: T1,
    remainingAttempts: 3,
    state: 'active',
    createdAt: T0,
    revision: 1,
  }
  const parsed = parseClientConnectCode(connectCode)
  assert.equal(parsed.codeDigest, DIGEST)
  assert.equal(Object.isFrozen(parsed), true)
  assert.equal(errorCodeOf(() => parseClientConnectCode({
    ...connectCode,
    code: '68421975',
  })), 'INVALID_SHAPE')
  assert.equal(errorCodeOf(() => parseClientConnectCode({
    ...connectCode,
    codeDigest: 'a'.repeat(64),
  })), 'INVALID_IDENTIFIER')
})

test('repository binding projection stays free of path fields', () => {
  const binding = {
    ...bindingProjection(),
    clientNodeId: crockfordId('cnd'),
    revision: 1,
  }
  const parsed = parseRepositoryBinding(binding)
  assert.equal(parsed.repositoryKind, 'git')
  assert.equal(parsed.headCommit, COMMIT)
  const withAbsolutePath = { ...binding, absolutePath: 'local/repository/sample' }
  assert.equal(errorCodeOf(() => parseRepositoryBinding(withAbsolutePath)), 'INVALID_SHAPE')
})

test('published connect code message rejects a plaintext code field', () => {
  const published = validMessage('client.connect_code.published', { code: '68421975' })
  assert.equal(
    errorCodeOf(() => parseClientToServerMessage(published)),
    'INVALID_SHAPE',
  )
})

test('domain helpers round-trip the schema scalars', () => {
  const report = parseClientCapacityReport(capacityReport())
  assert.deepEqual({ ...report }, capacityReport())
  const lease = {
    clientOccupancyLeaseId: crockfordId('ocl'),
    clientNodeId: crockfordId('cnd'),
    holderUserId: crockfordId('usr'),
    state: 'occupied',
    fencingToken: '12',
    claimRequestId: crockfordId('ocq'),
    claimedAt: T0,
    acknowledgedAt: T0,
    lastRenewedAt: null,
    idleExpiresAt: null,
    recoveryDeadlineAt: null,
    releaseRequestedAt: null,
    releasedAt: null,
    releaseReason: null,
    revision: 2,
  }
  const parsedLease = parseClientOccupancyLease(lease)
  assert.equal(parsedLease.fencingToken, '12')
  assert.equal(typeof parsedLease.fencingToken, 'string')
  assert.equal(
    errorCodeOf(() => parseClientOccupancyLease({ ...lease, fencingToken: 12 })),
    'INVALID_IDENTIFIER',
  )
  const controlError = parseClientControlError({
    code: 'STALE_FENCING_TOKEN',
    message: 'token superseded',
    retryable: false,
  })
  assert.equal(controlError.code, 'STALE_FENCING_TOKEN')
  assert.equal(
    errorCodeOf(() => parseClientControlError({
      code: 'STALE_FENCING_TOKEN',
      message: 'leak',
      retryable: false,
      details: { apiKey: 'sk-test' },
    })),
    'INVALID_VALUE',
  )
})
