import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { readFileSync, readdirSync } from 'node:fs'
import { join, resolve } from 'node:path'
import test from 'node:test'

import Ajv2020 from 'ajv/dist/2020.js'
import addFormats from 'ajv-formats'
import ts from 'typescript'

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

function propertyName(node) {
  if (!node) return null
  if (ts.isIdentifier(node) || ts.isStringLiteral(node) || ts.isNumericLiteral(node)) {
    return node.text
  }
  return null
}

function objectProperty(node, name) {
  return node.properties.find(property => (
    ts.isPropertyAssignment(property) && propertyName(property.name) === name
  ))
}

function generatedRuntimeSchemas(source, path) {
  const file = ts.createSourceFile(path, source, ts.ScriptTarget.Latest, true, ts.ScriptKind.TS)
  let initializer = null
  function visit(node) {
    if (ts.isVariableDeclaration(node)
      && ts.isIdentifier(node.name)
      && node.name.text === 'CONTROL_PLANE_RUNTIME_SCHEMAS') {
      initializer = node.initializer
    }
    ts.forEachChild(node, visit)
  }
  visit(file)
  assert.ok(initializer && ts.isObjectLiteralExpression(initializer))
  return JSON.parse(initializer.getText(file))
}

function assertCanonicalHandwrittenVerdictRequests(source, path) {
  const file = ts.createSourceFile(path, source, ts.ScriptTarget.Latest, true, ts.ScriptKind.TS)
  function assertCanonicalFields(properties, description) {
    const fields = properties.map(property => propertyName(property.name))
    assert.equal(
      fields.every(name => name !== null),
      true,
      `${path} cannot prove the fields in ${description}`,
    )
    assert.deepEqual(
      fields.sort(),
      ['candidateDigest', 'deliveryId'],
      `${path} constructs a non-canonical ${description}`,
    )
  }
  function inspectTypeMembers(members) {
    const commandMember = members.find(member => (
      ts.isPropertySignature(member) && propertyName(member.name) === 'command'
    ))
    if (!commandMember
      || !commandMember.type
      || !ts.isLiteralTypeNode(commandMember.type)
      || !ts.isStringLiteral(commandMember.type.literal)
      || commandMember.type.literal.text !== 'delivery.submit_verdict') return
    const payloadMember = members.find(member => (
      ts.isPropertySignature(member) && propertyName(member.name) === 'payload'
    ))
    assert.ok(
      payloadMember?.type && ts.isTypeLiteralNode(payloadMember.type),
      `${path} hand-maintains delivery.submit_verdict without an inspectable generated payload`,
    )
    assertCanonicalFields(payloadMember.type.members, 'delivery.submit_verdict payload type')
  }
  function visit(node) {
    if ((ts.isInterfaceDeclaration(node) || ts.isTypeAliasDeclaration(node))
      && /^(?:DeliverySubmitVerdictCommand|DeliverySubmitVerdictPayload)$/u.test(node.name.text)) {
      assert.fail(`${path} redeclares generated ${node.name.text}`)
    }
    if (ts.isInterfaceDeclaration(node)) inspectTypeMembers(node.members)
    if (ts.isTypeLiteralNode(node)) inspectTypeMembers(node.members)
    if (ts.isObjectLiteralExpression(node)) {
      const commandProperty = objectProperty(node, 'command')
      if (commandProperty
        && ts.isStringLiteral(commandProperty.initializer)
        && commandProperty.initializer.text === 'delivery.submit_verdict') {
        const payloadProperty = objectProperty(node, 'payload')
        if (payloadProperty && ts.isObjectLiteralExpression(payloadProperty.initializer)) {
          assertCanonicalFields(
            payloadProperty.initializer.properties,
            'delivery.submit_verdict payload',
          )
        }
      }
    }
    ts.forEachChild(node, visit)
  }
  visit(file)
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

  const generatedClientPath = join(
    root,
    'apps',
    'web',
    'src',
    'generated',
    'control-plane-client.ts',
  )
  const generatedClient = readFileSync(generatedClientPath, 'utf8')
  const runtimePayload = generatedRuntimeSchemas(generatedClient, generatedClientPath)
    .DeliverySubmitVerdictPayload
  assert.equal(runtimePayload.additionalProperties, false)
  assert.deepEqual([...runtimePayload.required].sort(), ['candidateDigest', 'deliveryId'])
  assert.deepEqual(
    Object.keys(runtimePayload.properties).sort(),
    ['candidateDigest', 'deliveryId'],
  )

  assert.doesNotThrow(() => assertCanonicalHandwrittenVerdictRequests(`
    const request = {
      command: 'delivery.submit_verdict',
      payload: { deliveryId, candidateDigest },
    }
  `, 'canonical-verdict-request.ts'))
  assert.throws(() => assertCanonicalHandwrittenVerdictRequests(`
    const request = {
      command: 'delivery.submit_verdict',
      payload: { deliveryId, candidateDigest, evidence: [] },
    }
  `, 'caller-derived-verdict-request.ts'), /non-canonical delivery\.submit_verdict payload/u)
  assert.throws(() => assertCanonicalHandwrittenVerdictRequests(`
    type HandwrittenVerdictCommand = {
      command: 'delivery.submit_verdict'
      payload: { deliveryId: string; candidateDigest: string; verdict: unknown }
    }
  `, 'caller-derived-verdict-type.ts'), /non-canonical delivery\.submit_verdict payload type/u)
  assert.throws(() => assertCanonicalHandwrittenVerdictRequests(`
    type DeliverySubmitVerdictPayload = { deliveryId: string; candidateDigest: string }
  `, 'duplicate-verdict-dto.ts'), /redeclares generated DeliverySubmitVerdictPayload/u)

  const webFiles = readdirSync(join(root, 'apps', 'web', 'src'), { recursive: true })
    .filter(path => typeof path === 'string' && /\.(?:ts|tsx)$/u.test(path))
    .filter(path => path.split(/[\\/]/u)[0] !== 'generated')
  for (const path of webFiles) {
    const source = readFileSync(join(root, 'apps', 'web', 'src', path), 'utf8')
    assertCanonicalHandwrittenVerdictRequests(source, path)
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
