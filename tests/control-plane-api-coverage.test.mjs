import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'
import { join, resolve } from 'node:path'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')
const matrixPath = join(root, 'docs', 'contracts', 'control-plane-api-coverage.matrix.json')
const inventoryPath = join(
  root,
  'docs',
  'decisions',
  '0028-control-plane-worker-migration.inventory.json',
)
const schemaRoot = join(root, 'schema', 'winwincode', 'v1')

async function json(path) {
  return JSON.parse(await readFile(path, 'utf8'))
}

function definition(schema, ref) {
  assert.match(ref, /^#\/\$defs\/[A-Za-z][A-Za-z0-9]*$/u)
  return schema.$defs[ref.slice('#/$defs/'.length)]
}

function specializations(schema, unionName, discriminator) {
  return schema.$defs[unionName].oneOf.map(({ $ref }) => {
    const branch = definition(schema, $ref)
    const object = branch.allOf?.find(entry => entry.type === 'object') ?? branch
    return object.properties[discriminator].const
  })
}

function sorted(values) {
  return [...values].sort()
}

function referencedContracts(matrix, field) {
  return new Set(matrix.flows.flatMap(flow => flow[field] ?? []))
}

function collectPropertyNames(value, names = []) {
  if (Array.isArray(value)) {
    for (const entry of value) collectPropertyNames(entry, names)
    return names
  }
  if (value === null || typeof value !== 'object') return names
  for (const property of Object.keys(value.properties ?? {})) names.push(property)
  for (const entry of Object.values(value)) collectPropertyNames(entry, names)
  return names
}

test('coverage matrix covers every migration caller and observable contract', async () => {
  const [matrix, inventory] = await Promise.all([
    json(matrixPath),
    json(inventoryPath),
  ])

  assert.equal(matrix.schemaVersion, 1)
  assert.equal(matrix.decision, 'ADR-0028')
  assert.equal(matrix.finalSharedDefinitionNames.executionPortModelRoute, 'ModelGatewayRoute')
  assert.deepEqual(
    sorted(matrix.callerCoverage.map(entry => entry.callerId)),
    sorted(inventory.callers.map(entry => entry.id)),
  )
  const flowIds = new Set(matrix.flows.map(flow => flow.id))
  for (const caller of matrix.callerCoverage) {
    assert.ok(caller.flowIds.length > 0, `${caller.callerId} has no flow`)
    for (const flowId of caller.flowIds) assert.ok(flowIds.has(flowId), flowId)
  }
  assert.deepEqual(
    sorted(matrix.surfaceCoverage.map(entry => entry.surfaceId)),
    sorted(inventory.surfaces.map(entry => entry.id)),
  )

  const surfaceCoverage = new Map(
    matrix.surfaceCoverage.map(entry => [entry.surfaceId, entry]),
  )
  for (const surface of inventory.surfaces) {
    const coverage = surfaceCoverage.get(surface.id)
    assert.ok(coverage, surface.id)
    assert.deepEqual(
      sorted(coverage.observableContracts),
      sorted(surface.observableContracts),
      surface.id,
    )
    assert.ok(coverage.flowIds.length > 0, `${surface.id} has no flow`)
  }
})

test('coverage matrix exactly matches all public transport unions', async () => {
  const [matrix, http, websocket, executionPort] = await Promise.all([
    json(matrixPath),
    json(join(schemaRoot, 'control-plane-http.schema.json')),
    json(join(schemaRoot, 'control-plane-events.schema.json')),
    json(join(schemaRoot, 'execution-port.schema.json')),
  ])

  assert.deepEqual(
    sorted(matrix.contracts.httpCommands),
    sorted(specializations(http, 'CommandRequest', 'command')),
  )
  assert.deepEqual(
    sorted(matrix.contracts.httpQueries),
    sorted(specializations(http, 'QueryRequest', 'query')),
  )
  assert.deepEqual(
    sorted(matrix.contracts.websocketEvents),
    sorted(specializations(websocket, 'ControlPlaneWebSocketEventPayload', 'type')),
  )
  assert.deepEqual(
    sorted(matrix.contracts.executionPortMessages),
    sorted(specializations(executionPort, 'ExecutionPortMessage', 'kind')),
  )

  for (const field of [
    'httpCommands',
    'httpQueries',
    'websocketEvents',
    'executionPortMessages',
  ]) {
    assert.deepEqual(
      sorted(referencedContracts(matrix, field)),
      sorted(matrix.contracts[field]),
      `${field} contains an unmapped public branch`,
    )
  }
})

test('coverage matrix freezes authority and secret-safe boundaries', async () => {
  const [matrix, http, websocket, executionPort] = await Promise.all([
    json(matrixPath),
    json(join(schemaRoot, 'control-plane-http.schema.json')),
    json(join(schemaRoot, 'control-plane-events.schema.json')),
    json(join(schemaRoot, 'execution-port.schema.json')),
  ])
  const publicProperties = collectPropertyNames({ http, websocket, executionPort })

  for (const property of matrix.authorityBoundaries.forbiddenPublicProperties) {
    assert.equal(
      publicProperties.includes(property),
      false,
      `public contract exposes ${property}`,
    )
  }
  assert.deepEqual(matrix.authorityBoundaries.allowedCodexReferences, ['codexThreadId'])
  for (const property of publicProperties.filter(name => /codex/iu.test(name))) {
    assert.equal(
      matrix.authorityBoundaries.allowedCodexReferences.includes(property),
      true,
      `public contract exposes Codex internal property ${property}`,
    )
  }
  assert.equal(
    websocket.$defs.ControlPlaneWebSocketClientFrame.oneOf.some(branch => (
      /Command/u.test(branch.$ref)
    )),
    false,
  )
  assert.equal(
    executionPort['x-winwincode-semantics'].workerAuthority.effect,
    'canonical_product_state_remains_control_plane_owned',
  )
})
