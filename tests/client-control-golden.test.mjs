import assert from 'node:assert/strict'
import { readdirSync, readFileSync } from 'node:fs'
import { join, resolve } from 'node:path'
import test from 'node:test'

// Golden contract for the ClientControlPort envelope (multi-user client plan,
// sections 9.3, 9.4, 9.5). Kind strings are copied verbatim from the plan.
//
// Fixtures: tests/fixtures/client-control/<kind>.json (one valid envelope per
// protocol kind) and tests/fixtures/client-control/invalid/*.json (samples the
// validator must reject).

const root = resolve(import.meta.dirname, '..')
const fixturesDir = join(root, 'tests', 'fixtures', 'client-control')
const invalidDir = join(fixturesDir, 'invalid')

const CLIENT_TO_SERVER_KINDS = Object.freeze([
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

const SERVER_TO_CLIENT_KINDS = Object.freeze([
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

const ALL_KINDS = Object.freeze([
  ...CLIENT_TO_SERVER_KINDS,
  ...SERVER_TO_CLIENT_KINDS,
])

const KIND_TO_DIRECTION = Object.freeze(new Map([
  ...CLIENT_TO_SERVER_KINDS.map(kind => [kind, 'client-to-server']),
  ...SERVER_TO_CLIENT_KINDS.map(kind => [kind, 'server-to-client']),
]))

// Pure-fact deliveries carry no command fields. Per the frozen protocol
// contract (docs/contracts/client-control-port-v1.md), client.heartbeat is the
// only such kind: every other envelope in both directions is a command and
// must carry expectedRevision + idempotencyKey.
const FACT_KINDS = Object.freeze(new Set([
  'client.heartbeat',
]))

// Repository traffic is stamped only when an active occupancy lease exists
// (`C + L（活动占用时）`), so its fencing fields are optional rather than
// forbidden.
const OPTIONAL_FENCING_KINDS = Object.freeze(new Set([
  'client.repository.upsert',
  'client.repository.removed',
  'client.repository.status',
  'client.repository.rescan',
]))

// Occupancy, worker, and candidate traffic always rides on an occupancy lease,
// so every such envelope carries occupancyLeaseId + occupancyFencingToken.
const OCCUPANCY_CLASS_PATTERN = /(?:^|\.)(?:occupancy|worker|candidate)(?:\.|$)/u

const SCHEMA_VERSION = 'winwincode/v1'
const ENVELOPE_REQUIRED_FIELDS = Object.freeze([
  'schemaVersion',
  'messageId',
  'clientNodeId',
  'clientInstanceId',
  'sequence',
  'occurredAt',
  'kind',
  'payload',
])
const ISO_TIMESTAMP_PATTERN = /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$/u
const COMMAND_FIELDS = Object.freeze(['expectedRevision', 'idempotencyKey'])
const FENCING_FIELDS = Object.freeze(['occupancyLeaseId', 'occupancyFencingToken'])
// Mirrors ExecutionPort's FencingToken $def: a monotonically increasing
// decimal token encoded as a string to preserve 64-bit precision.
const FENCING_TOKEN_PATTERN = /^[1-9][0-9]{0,19}$/u

function isFilledString(value) {
  return typeof value === 'string' && value.length > 0
}

function isAbsolutePathText(value) {
  return value.startsWith('/') || /^[A-Za-z]:[\\/]/u.test(value)
}

// Only the invalid lane may demonstrate an absolute path; valid fixtures must
// keep every path-shaped string relative or a "<device-local-path>" placeholder.
const EMBEDDED_ABSOLUTE_PATH_PATTERN =
  /(?:^|[\s("])\/(?:Users|home|tmp|var|etc|opt|usr|Volumes)\//u

function collectPayloadStrings(value, found = []) {
  if (value === null || typeof value !== 'object') {
    if (typeof value === 'string') found.push(value)
    return found
  }
  for (const child of Object.values(value)) collectPayloadStrings(child, found)
  return found
}

function isOccupancyClass(kind) {
  return OCCUPANCY_CLASS_PATTERN.test(kind)
}

// Structural validator for one ClientControlPort envelope. Returns a sorted
// list of violation codes; an empty list means the envelope is contract-clean.
function collectViolations(envelope) {
  const violations = []

  for (const field of ENVELOPE_REQUIRED_FIELDS) {
    const value = envelope[field]
    if (value === undefined || value === null || value === '') {
      violations.push(`envelope.missing_field:${field}`)
    }
  }

  const kind = envelope.kind
  if (!isFilledString(kind) || !KIND_TO_DIRECTION.has(kind)) {
    violations.push('kind.unknown')
    return violations.sort()
  }

  if (envelope.schemaVersion !== SCHEMA_VERSION) {
    violations.push('envelope.invalid_schemaVersion')
  }
  if (!Number.isInteger(envelope.sequence) || envelope.sequence < 0) {
    violations.push('envelope.invalid_sequence')
  }
  if (
    !isFilledString(envelope.occurredAt) ||
    !ISO_TIMESTAMP_PATTERN.test(envelope.occurredAt)
  ) {
    violations.push('envelope.invalid_occurredAt')
  }
  if (
    envelope.payload === null ||
    typeof envelope.payload !== 'object' ||
    Array.isArray(envelope.payload)
  ) {
    violations.push('envelope.invalid_payload')
  }

  if (FACT_KINDS.has(kind)) {
    for (const field of COMMAND_FIELDS) {
      if (envelope[field] !== undefined) {
        violations.push(`fact.unexpected_field:${field}`)
      }
    }
  } else {
    if (!Number.isInteger(envelope.expectedRevision) || envelope.expectedRevision < 0) {
      violations.push('command.missing_expectedRevision')
    }
    if (!isFilledString(envelope.idempotencyKey)) {
      violations.push('command.missing_idempotencyKey')
    }
  }

  if (isOccupancyClass(kind)) {
    if (!isFilledString(envelope.occupancyLeaseId)) {
      violations.push('fencing.missing:occupancyLeaseId')
    }
    if (
      !isFilledString(envelope.occupancyFencingToken) ||
      !FENCING_TOKEN_PATTERN.test(envelope.occupancyFencingToken)
    ) {
      violations.push('fencing.missing:occupancyFencingToken')
    }
  } else if (!OPTIONAL_FENCING_KINDS.has(kind)) {
    for (const field of FENCING_FIELDS) {
      if (envelope[field] !== undefined) {
        violations.push(`fencing.unexpected_field:${field}`)
      }
    }
  }

  for (const text of collectPayloadStrings(envelope.payload)) {
    if (isAbsolutePathText(text) || EMBEDDED_ABSOLUTE_PATH_PATTERN.test(text)) {
      violations.push('path.absolute')
    }
  }

  return violations.sort()
}

function readJson(path) {
  return JSON.parse(readFileSync(path, 'utf8'))
}

const validFiles = readdirSync(fixturesDir, { withFileTypes: true })
  .filter(entry => entry.isFile() && entry.name.endsWith('.json'))
  .map(entry => entry.name)
  .sort()

const invalidFiles = readdirSync(invalidDir, { withFileTypes: true })
  .filter(entry => entry.isFile() && entry.name.endsWith('.json'))
  .map(entry => entry.name)
  .sort()

test('kind registry matches the ClientControlPort plan verbatim', () => {
  assert.equal(CLIENT_TO_SERVER_KINDS.length, 16)
  assert.equal(SERVER_TO_CLIENT_KINDS.length, 11)
  assert.equal(new Set(ALL_KINDS).size, ALL_KINDS.length)
})

test('golden fixtures cover every protocol kind exactly once, named by kind', () => {
  const expectedFiles = ALL_KINDS.map(kind => `${kind}.json`).sort()
  assert.deepEqual(validFiles, expectedFiles)

  for (const file of validFiles) {
    const envelope = readJson(join(fixturesDir, file))
    assert.equal(
      envelope.kind,
      file.replace(/\.json$/u, ''),
      `fixture filename must equal its envelope kind: ${file}`,
    )
  }
})

test('every valid envelope satisfies the structural contract', () => {
  for (const file of validFiles) {
    const envelope = readJson(join(fixturesDir, file))
    assert.deepEqual(
      collectViolations(envelope),
      [],
      `${file} must be a contract-clean envelope`,
    )
    const direction = KIND_TO_DIRECTION.get(envelope.kind)
    assert.ok(
      direction === 'client-to-server' || direction === 'server-to-client',
      `${file} kind must belong to a protocol direction`,
    )
  }
})

test('command envelopes carry expectedRevision and idempotencyKey', () => {
  for (const file of validFiles) {
    const envelope = readJson(join(fixturesDir, file))
    if (FACT_KINDS.has(envelope.kind)) {
      assert.equal(envelope.expectedRevision, undefined, file)
      assert.equal(envelope.idempotencyKey, undefined, file)
      continue
    }
    assert.equal(
      Number.isInteger(envelope.expectedRevision) && envelope.expectedRevision >= 0,
      true,
      `${file} needs a non-negative integer expectedRevision`,
    )
    assert.equal(
      isFilledString(envelope.idempotencyKey),
      true,
      `${file} needs an idempotencyKey`,
    )
  }
})

test('occupancy, worker, and candidate envelopes carry lease fencing fields', () => {
  for (const file of validFiles) {
    const envelope = readJson(join(fixturesDir, file))
    if (!isOccupancyClass(envelope.kind)) {
      assert.equal(envelope.occupancyLeaseId, undefined, file)
      assert.equal(envelope.occupancyFencingToken, undefined, file)
      continue
    }
    assert.equal(
      isFilledString(envelope.occupancyLeaseId),
      true,
      `${file} needs occupancyLeaseId`,
    )
    assert.equal(
      isFilledString(envelope.occupancyFencingToken) &&
        FENCING_TOKEN_PATTERN.test(envelope.occupancyFencingToken),
      true,
      `${file} needs a decimal-string occupancyFencingToken`,
    )
  }
})

test('valid fixtures stay deterministic and never contain absolute paths', () => {
  const messageIds = new Set()

  for (const file of validFiles) {
    const envelope = readJson(join(fixturesDir, file))
    assert.equal(
      messageIds.has(envelope.messageId),
      false,
      `messageId must be unique across golden fixtures, reused in ${file}`,
    )
    messageIds.add(envelope.messageId)

    for (const text of collectPayloadStrings(envelope.payload)) {
      assert.equal(
        isAbsolutePathText(text),
        false,
        `${file} payload must not contain absolute paths, found: ${text}`,
      )
      assert.equal(
        EMBEDDED_ABSOLUTE_PATH_PATTERN.test(text),
        false,
        `${file} payload must not embed absolute paths, found: ${text}`,
      )
    }
  }
})

test('invalid fixtures are rejected by the validator with the expected violations', () => {
  assert.deepEqual(invalidFiles, [
    'absolute-path-payload.json',
    'missing-fencing.json',
    'unknown-kind.json',
  ])

  for (const file of invalidFiles) {
    const sample = readJson(join(invalidDir, file))
    assert.equal(isFilledString(sample.name), true, `${file} needs a name`)
    assert.deepEqual(
      collectViolations(sample.envelope),
      sample.expectedViolations,
      `${file} must be rejected for exactly the documented violations`,
    )
  }
})
