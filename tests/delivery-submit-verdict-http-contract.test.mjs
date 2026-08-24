import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { readFileSync, readdirSync } from 'node:fs'
import { join, resolve } from 'node:path'
import test from 'node:test'

import Ajv2020 from 'ajv/dist/2020.js'
import addFormats from 'ajv-formats'

const root = resolve(import.meta.dirname, '..')
const schemaRoot = join(root, 'schema', 'winwincode', 'v1')
const http = JSON.parse(readFileSync(join(schemaRoot, 'control-plane-http.schema.json'), 'utf8'))
const domain = JSON.parse(readFileSync(join(schemaRoot, 'domain.schema.json'), 'utf8'))

function validator() {
  const ajv = new Ajv2020({ allErrors: true, strict: true })
  addFormats(ajv)
  for (const [keyword, schemaType] of [
    ['x-authority', 'string'],
    ['x-direction', 'string'],
    ['x-winwincode-openapi', 'object'],
    ['x-winwincode-semantics', 'object'],
    ['x-winwincode-transports', 'object'],
  ]) ajv.addKeyword({ keyword, schemaType, valid: true })
  for (const name of [
    'domain.schema.json',
    'control-plane-http.schema.json',
    'control-plane-events.schema.json',
    'execution-port.schema.json',
  ]) {
    ajv.addSchema(JSON.parse(readFileSync(join(schemaRoot, name), 'utf8')))
  }
  return ajv.getSchema(`${http.$id}#/$defs/DeliverySubmitVerdictCommand`)
}

function command() {
  return {
    schemaVersion: 'winwincode/v1',
    command: 'delivery.submit_verdict',
    actor: {
      kind: 'user',
      id: 'usr_01J00000000000000000000000',
    },
    scope: {
      kind: 'repository',
      organizationId: 'org_01J00000000000000000000000',
      workspaceId: 'wsp_01J00000000000000000000000',
      projectId: 'prj_01J00000000000000000000000',
      repositoryId: 'rep_01J00000000000000000000000',
    },
    requestId: 'req_01J00000000000000000000000',
    expectedRevision: 7,
    payload: {
      deliveryId: 'dlv_01J00000000000000000000000',
      candidateDigest: `sha256:${'a'.repeat(64)}`,
    },
  }
}

function copy(value) {
  return structuredClone(value)
}

test('submit verdict accepts one stale-check request and rejects caller-derived facts', () => {
  const validate = validator()
  assert.ok(validate)
  assert.equal(validate(command()), true, JSON.stringify(validate.errors))

  const payload = http.$defs.DeliverySubmitVerdictPayload
  assert.equal(payload.additionalProperties, false)
  assert.deepEqual(payload.required, ['deliveryId', 'candidateDigest'])
  assert.deepEqual(Object.keys(payload.properties).sort(), ['candidateDigest', 'deliveryId'])
  assert.match(payload.properties.candidateDigest.description, /stale-check only/u)

  for (const field of [
    'attention',
    'credential',
    'criterionResults',
    'evidence',
    'rawRuntimeFacts',
    'runtimeEvents',
    'status',
    'verdict',
    'verification',
  ]) {
    const untrusted = copy(command())
    untrusted.payload[field] = {}
    assert.equal(validate(untrusted), false, `accepted caller field ${field}`)
  }

  const unknownEnvelope = copy(command())
  unknownEnvelope.legacyVerdict = { status: 'pass' }
  assert.equal(validate(unknownEnvelope), false)
})

test('submit verdict binds actor, repository scope, request identity, and revision', () => {
  const validate = validator()
  for (const field of [
    'schemaVersion',
    'command',
    'actor',
    'scope',
    'requestId',
    'expectedRevision',
    'payload',
  ]) {
    const incomplete = copy(command())
    delete incomplete[field]
    assert.equal(validate(incomplete), false, `accepted missing envelope field ${field}`)
  }

  const partialScope = copy(command())
  delete partialScope.scope.repositoryId
  assert.equal(validate(partialScope), false)

  const workspaceScope = copy(command())
  workspaceScope.scope = {
    kind: 'workspace',
    organizationId: workspaceScope.scope.organizationId,
    workspaceId: workspaceScope.scope.workspaceId,
  }
  assert.equal(validate(workspaceScope), false, 'Delivery verdict must bind repository ownership')
})

test('submit verdict errors and replay behavior are externally distinguishable', () => {
  assert.deepEqual(http['x-winwincode-semantics'].verdictSubmission, {
    authority: 'control_plane_sealed_facts_only',
    candidateDigest: 'stale_check_only',
    payload: ['deliveryId', 'candidateDigest'],
    receiptIdentity: 'actor+repository_scope+requestId',
    identicalRetry: 'return_original_http_status_and_body_without_recomputation',
    errors: {
      requestConflict: 'IDEMPOTENCY_CONFLICT',
      revisionConflict: 'REVISION_CONFLICT',
      staleCandidate: 'CANDIDATE_STALE',
      trustedFactsUnavailable: 'TRUSTED_FACTS_UNAVAILABLE',
    },
  })
  assert.deepEqual(http['x-winwincode-semantics'].errors.CANDIDATE_STALE, {
    httpStatus: 409,
    retryable: false,
  })
  assert.deepEqual(http['x-winwincode-semantics'].errors.TRUSTED_FACTS_UNAVAILABLE, {
    httpStatus: 503,
    retryable: true,
  })
  assert.ok(domain.$defs.TerminalErrorCode.enum.includes('CANDIDATE_STALE'))
  assert.ok(domain.$defs.RetryableErrorCode.enum.includes('TRUSTED_FACTS_UNAVAILABLE'))
})

test('all generated clients and docs expose only the canonical verdict request', () => {
  const generated = spawnSync(process.execPath, [
    join(root, 'scripts', 'generate-contracts.mjs'),
    '--check',
  ], { cwd: root, encoding: 'utf8' })
  assert.equal(generated.status, 0, `${generated.stdout}\n${generated.stderr}`)

  const rust = readFileSync(join(root, 'crates', 'winwincode-api', 'src', 'generated.rs'), 'utf8')
  const typescript = readFileSync(join(root, 'apps', 'web', 'src', 'generated', 'contracts.ts'), 'utf8')
  const openapi = JSON.parse(readFileSync(join(schemaRoot, 'openapi.generated.json'), 'utf8'))
  const collection = JSON.parse(
    readFileSync(join(schemaRoot, 'schema-collection.generated.json'), 'utf8'),
  )
  for (const source of [rust, typescript, JSON.stringify(openapi), JSON.stringify(collection)]) {
    assert.doesNotMatch(source, /CriterionVerdictInput|criterionResults/u)
  }
  assert.deepEqual(
    Object.keys(openapi.components.schemas.DeliverySubmitVerdictPayload.properties).sort(),
    ['candidateDigest', 'deliveryId'],
  )
  assert.deepEqual(
    Object.keys(collection.$defs.DeliverySubmitVerdictPayload.properties).sort(),
    ['candidateDigest', 'deliveryId'],
  )

  const webFiles = readdirSync(join(root, 'apps', 'web', 'src'), { recursive: true })
    .filter(path => typeof path === 'string' && /\.(?:ts|tsx)$/u.test(path))
    .filter(path => path !== join('generated', 'contracts.ts'))
  for (const path of webFiles) {
    const source = readFileSync(join(root, 'apps', 'web', 'src', path), 'utf8')
    assert.doesNotMatch(source, /DeliverySubmitVerdict(?:Command|Payload)|candidateDigest/u)
  }

  const coverage = readFileSync(
    join(root, 'docs', 'contracts', 'control-plane-api-coverage.matrix.json'),
    'utf8',
  )
  assert.doesNotMatch(coverage, /submit candidate-bound criterion results/u)
  assert.match(coverage, /request server-computed verdicts for one candidate stale-check digest/u)

  const contractDoc = readFileSync(
    join(root, 'docs', 'contracts', 'delivery-evidence-verdict-rework.md'),
    'utf8',
  )
  assert.match(contractDoc, /`candidateDigest` 只用于发现候选已经变化/u)
  assert.match(contractDoc, /`IDEMPOTENCY_CONFLICT`/u)
  assert.match(contractDoc, /`REVISION_CONFLICT`/u)
  assert.match(contractDoc, /`CANDIDATE_STALE`/u)
  assert.match(contractDoc, /`TRUSTED_FACTS_UNAVAILABLE`/u)
})
