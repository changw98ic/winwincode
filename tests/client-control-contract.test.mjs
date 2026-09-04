import assert from 'node:assert/strict'
import test from 'node:test'

import {
  CLIENT_CONTROL_SCHEMA_VERSION,
  CLIENT_TO_SERVER_MESSAGE_KINDS,
  CLIENT_TO_SERVER_OCCUPANCY_FENCED_MESSAGE_KINDS,
  SERVER_TO_CLIENT_MESSAGE_KINDS,
  SERVER_TO_CLIENT_OCCUPANCY_FENCED_MESSAGE_KINDS,
  parseClientConnectCode,
  parseClientToServerMessage,
  parseRepositoryBinding,
  parseServerToClientMessage,
} from '../packages/contracts/dist/index.js'

const now = 1_800_000_000_000
const digest = 'a'.repeat(64)
const commit = 'b'.repeat(40)

// §9.3 Client → Server message kinds, verbatim from the approved plan.
const PLAN_CLIENT_TO_SERVER_KINDS = Object.freeze([
  'client.enroll',
  'client.hello',
  'client.heartbeat',
  'client.connect_code.published',
  'client.access.challenge_ack',
  'client.occupancy.ack',
  'client.occupancy.rejected',
  'client.repository.upsert',
  'client.repository.removed',
  'client.repository.status',
  'client.worker.launch_ack',
  'client.worker.state',
  'client.worker.reconcile',
  'client.candidate.retained',
  'client.candidate.apply_result',
  'client.command_ack',
])

// §9.4 Server → Client message kinds, verbatim from the approved plan.
const PLAN_SERVER_TO_CLIENT_KINDS = Object.freeze([
  'client.enrollment_accepted',
  'client.access.challenge',
  'client.occupancy.offer',
  'client.occupancy.release',
  'client.occupancy.force_fence',
  'client.repository.rescan',
  'client.worker.launch',
  'client.worker.stop',
  'client.candidate.apply',
  'client.client_lock',
  'client.credential_rotate',
])

function clientNodeEnrollmentFixture() {
  return {
    publicClientId: '927351842',
    displayName: 'Wen-ge MacBook Pro',
    platform: 'darwin',
    architecture: 'aarch64',
    clientVersion: '0.1.0-alpha.1',
    deviceCredentialDigest: digest,
    maxConcurrentWorkerSessions: 8,
  }
}

function heartbeatPayloadFixture() {
  return {
    expectedRevision: 3,
    idempotencyKey: 'idempotency-heartbeat-1',
    presenceState: 'online',
    acceptingConnections: true,
    lockState: 'unlocked',
    maxConcurrentWorkerSessions: 8,
    reportedRunningWorkerSessions: 2,
  }
}

function connectCodeFixture() {
  return {
    connectCodeId: 'connect-code-1',
    clientNodeId: 'client-node-1',
    codeDigest: digest,
    issuedByInstanceId: 'client-instance-1',
    expiresAt: now + 120_000,
    remainingAttempts: 3,
    state: 'active',
    createdAt: now,
    revision: 1,
  }
}

function repositoryBindingFixture() {
  return {
    repositoryBindingId: 'repository-binding-1',
    clientNodeId: 'client-node-1',
    displayName: 'WinWinCode',
    repositoryKind: 'git',
    defaultBranch: 'main',
    headCommit: commit,
    dirtyState: 'clean',
    availability: 'available',
    repositoryFingerprint: digest,
    lastScannedAt: now,
    revision: 1,
  }
}

function workerLaunchGrantFixture() {
  return {
    workerLaunchGrantId: 'worker-launch-grant-1',
    clientNodeId: 'client-node-1',
    clientInstanceId: 'client-instance-1',
    occupancyLeaseId: 'occupancy-lease-1',
    occupancyFencingToken: 4,
    repositoryBindingId: 'repository-binding-1',
    productSessionId: 'product-session-1',
    stageRunId: 'stage-run-1',
    workerSessionId: 'worker-session-1',
    workerId: 'worker-1',
    workerInstanceId: 'worker-instance-1',
    credentialDigest: digest,
    expiresAt: now + 60_000,
    state: 'issued',
    revision: 1,
  }
}

function envelopeFixture(kind, payload) {
  return {
    schemaVersion: CLIENT_CONTROL_SCHEMA_VERSION,
    messageId: 'message-0001',
    clientNodeId: 'client-node-1',
    clientInstanceId: 'client-instance-1',
    sequence: 7,
    occurredAt: now,
    kind,
    payload,
  }
}

test('client-to-server message kinds match the plan verbatim', () => {
  assert.deepEqual([...CLIENT_TO_SERVER_MESSAGE_KINDS], [...PLAN_CLIENT_TO_SERVER_KINDS])
})

test('server-to-client message kinds match the plan verbatim', () => {
  assert.deepEqual([...SERVER_TO_CLIENT_MESSAGE_KINDS], [...PLAN_SERVER_TO_CLIENT_KINDS])
})

test('kind registries are frozen and direction-disjoint', () => {
  assert.equal(Object.isFrozen(CLIENT_TO_SERVER_MESSAGE_KINDS), true)
  assert.equal(Object.isFrozen(SERVER_TO_CLIENT_MESSAGE_KINDS), true)
  const overlap = PLAN_CLIENT_TO_SERVER_KINDS.filter(kind => (
    PLAN_SERVER_TO_CLIENT_KINDS.includes(kind)
  ))
  assert.deepEqual(overlap, [])
  for (const kind of CLIENT_TO_SERVER_OCCUPANCY_FENCED_MESSAGE_KINDS) {
    assert.equal(PLAN_CLIENT_TO_SERVER_KINDS.includes(kind), true)
  }
  for (const kind of SERVER_TO_CLIENT_OCCUPANCY_FENCED_MESSAGE_KINDS) {
    assert.equal(PLAN_SERVER_TO_CLIENT_KINDS.includes(kind), true)
  }
})

test('heartbeat command round-trips through the client-to-server parser', () => {
  const parsed = parseClientToServerMessage(
    envelopeFixture('client.heartbeat', heartbeatPayloadFixture()),
  )
  assert.equal(parsed.schemaVersion, CLIENT_CONTROL_SCHEMA_VERSION)
  assert.equal(parsed.messageId, 'message-0001')
  assert.equal(parsed.clientNodeId, 'client-node-1')
  assert.equal(parsed.clientInstanceId, 'client-instance-1')
  assert.equal(parsed.sequence, 7)
  assert.equal(parsed.occurredAt, now)
  assert.equal(parsed.kind, 'client.heartbeat')
  assert.deepEqual(parsed.payload, heartbeatPayloadFixture())
  assert.equal(Object.isFrozen(parsed), true)
  assert.equal(Object.isFrozen(parsed.payload), true)
})

test('every client-to-server kind rejects a payload without the command base', () => {
  for (const kind of CLIENT_TO_SERVER_MESSAGE_KINDS) {
    assert.throws(
      () => parseClientToServerMessage(envelopeFixture(kind, {})),
      error => error.code === 'INVALID_SHAPE',
      `kind ${kind} must require expectedRevision and idempotencyKey`,
    )
  }
})

test('every server-to-client kind rejects a payload without the command base', () => {
  for (const kind of SERVER_TO_CLIENT_MESSAGE_KINDS) {
    assert.throws(
      () => parseServerToClientMessage(envelopeFixture(kind, {})),
      error => error.code === 'INVALID_SHAPE',
      `kind ${kind} must require expectedRevision and idempotencyKey`,
    )
  }
})

test('occupancy-fenced commands require the lease identity pair', () => {
  const base = {
    expectedRevision: 1,
    idempotencyKey: 'idempotency-ack-1',
  }
  const missingPair = { ...base, acknowledgedAt: now }
  assert.throws(
    () => parseClientToServerMessage(envelopeFixture('client.occupancy.ack', missingPair)),
    error => error.code === 'INVALID_SHAPE',
  )
  const full = {
    ...base,
    occupancyLeaseId: 'occupancy-lease-1',
    occupancyFencingToken: 4,
    acknowledgedAt: now,
  }
  const parsed = parseClientToServerMessage(envelopeFixture('client.occupancy.ack', full))
  assert.equal(parsed.payload.occupancyLeaseId, 'occupancy-lease-1')
  assert.equal(parsed.payload.occupancyFencingToken, 4)
})

test('server-to-client fenced commands require the lease identity pair', () => {
  const missingPair = {
    expectedRevision: 1,
    idempotencyKey: 'idempotency-stop-1',
    workerSessionId: 'worker-session-1',
    reason: null,
  }
  assert.throws(
    () => parseServerToClientMessage(envelopeFixture('client.worker.stop', missingPair)),
    error => error.code === 'INVALID_SHAPE',
  )
})

test('non-fenced commands reject occupancy fencing fields', () => {
  const payload = {
    ...heartbeatPayloadFixture(),
    occupancyLeaseId: 'occupancy-lease-1',
    occupancyFencingToken: 4,
  }
  assert.throws(
    () => parseClientToServerMessage(envelopeFixture('client.heartbeat', payload)),
    error => error.code === 'INVALID_SHAPE',
  )
  const rotate = {
    expectedRevision: 1,
    idempotencyKey: 'idempotency-rotate-1',
    reason: null,
    occupancyLeaseId: 'occupancy-lease-1',
    occupancyFencingToken: 4,
  }
  assert.throws(
    () => parseServerToClientMessage(envelopeFixture('client.credential_rotate', rotate)),
    error => error.code === 'INVALID_SHAPE',
  )
})

test('worker launch command carries the fenced grant and round-trips', () => {
  const grant = workerLaunchGrantFixture()
  const payload = {
    expectedRevision: 2,
    idempotencyKey: 'idempotency-launch-1',
    occupancyLeaseId: grant.occupancyLeaseId,
    occupancyFencingToken: grant.occupancyFencingToken,
    grant,
  }
  const parsed = parseServerToClientMessage(envelopeFixture('client.worker.launch', payload))
  assert.equal(parsed.kind, 'client.worker.launch')
  assert.equal(parsed.payload.grant.workerSessionId, 'worker-session-1')
  assert.equal(parsed.payload.grant.occupancyLeaseId, 'occupancy-lease-1')
  assert.equal(parsed.payload.grant.occupancyFencingToken, 4)
  assert.equal(Object.isFrozen(parsed.payload), true)
})

test('worker launch command rejects a grant bound to another lease', () => {
  const grant = workerLaunchGrantFixture()
  const payload = {
    expectedRevision: 2,
    idempotencyKey: 'idempotency-launch-2',
    occupancyLeaseId: 'occupancy-lease-2',
    occupancyFencingToken: grant.occupancyFencingToken,
    grant,
  }
  assert.throws(
    () => parseServerToClientMessage(envelopeFixture('client.worker.launch', payload)),
    error => error.code === 'RELATIONSHIP_MISMATCH',
  )
})

test('wrong-direction kinds are rejected', () => {
  assert.throws(
    () => parseClientToServerMessage(
      envelopeFixture('client.occupancy.offer', {}),
    ),
    error => error.code === 'INVALID_VALUE',
  )
  assert.throws(
    () => parseServerToClientMessage(
      envelopeFixture('client.enroll', {}),
    ),
    error => error.code === 'INVALID_VALUE',
  )
})

test('envelope rejects an unsupported schema version', () => {
  const message = {
    ...envelopeFixture('client.heartbeat', heartbeatPayloadFixture()),
    schemaVersion: CLIENT_CONTROL_SCHEMA_VERSION + 1,
  }
  assert.throws(
    () => parseClientToServerMessage(message),
    error => error.code === 'UNSUPPORTED_SCHEMA_VERSION',
  )
})

test('connect code contract carries only the digest, never plaintext', () => {
  const parsed = parseClientConnectCode(connectCodeFixture())
  assert.equal(parsed.codeDigest, digest)
  assert.equal(Object.isFrozen(parsed), true)
  const withPlaintext = { ...connectCodeFixture(), code: '68421975' }
  assert.throws(
    () => parseClientConnectCode(withPlaintext),
    error => error.code === 'INVALID_SHAPE',
  )
  const withoutDigest = { ...connectCodeFixture() }
  delete withoutDigest.codeDigest
  assert.throws(
    () => parseClientConnectCode(withoutDigest),
    error => error.code === 'INVALID_SHAPE',
  )
})

test('repository binding rejects absolute path fields', () => {
  const parsed = parseRepositoryBinding(repositoryBindingFixture())
  assert.equal(parsed.repositoryKind, 'git')
  assert.equal(parsed.headCommit, commit)
  const withAbsolutePath = {
    ...repositoryBindingFixture(),
    absolutePath: '/local/repository/sample',
  }
  assert.throws(
    () => parseRepositoryBinding(withAbsolutePath),
    error => error.code === 'INVALID_SHAPE',
  )
})

test('enroll command projects path-free enrollment facts only', () => {
  const payload = {
    expectedRevision: 0,
    idempotencyKey: 'idempotency-enroll-1',
    node: clientNodeEnrollmentFixture(),
  }
  const parsed = parseClientToServerMessage(envelopeFixture('client.enroll', payload))
  assert.equal(parsed.kind, 'client.enroll')
  assert.equal(parsed.payload.node.publicClientId, '927351842')
  assert.equal(Object.isFrozen(parsed.payload.node), true)
  const withAbsolutePath = {
    ...payload,
    node: { ...clientNodeEnrollmentFixture(), sourceDirectory: '/local/repository/sample' },
  }
  assert.throws(
    () => parseClientToServerMessage(envelopeFixture('client.enroll', withAbsolutePath)),
    error => error.code === 'INVALID_SHAPE',
  )
})
