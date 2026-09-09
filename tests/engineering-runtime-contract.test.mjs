import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import test from 'node:test'
import Ajv from 'ajv/dist/2020.js'
import addFormats from 'ajv-formats'

const schema = JSON.parse(readFileSync(new URL('../schema/winwincode/v1/domain.schema.json', import.meta.url)))
const ajv = new Ajv({ strict: false, allErrors: true })
addFormats(ajv)
ajv.addSchema(schema)
const id = prefix => `${prefix}_01J00000000000000000000000`
const common = { schemaVersion: 'winwincode/v1' }
const contract = { workContractId: id('wct'), contractRevision: 1 }
const run = { workItemId: id('wit'), workRunId: id('wrn') }
const digest = `sha256:${'a'.repeat(64)}`
const identity = {
  executionJobId: id('job'), attempt: 1, workerId: id('wrk'),
  workerInstanceId: id('wki'), workerSessionId: id('wsn'),
  leaseId: id('lse'), fencingToken: '1',
}
const samples = {
  WorkContract: { ...common, id: id('wct'), revision: 1, scope: ['repository'], objective: 'Deliver tested change', constraints: [], protectedScope: ['credentials'], requiredHumanAuthority: 'none', criteria: [{ id: id('crt'), description: 'Test passes', required: true, requiredEvidenceClass: 'machine', verificationMethod: 'pnpm test' }], createdAt: '2026-09-08T00:00:00.000Z' },
  WorkItem: { ...common, id: id('wit'), workContractId: id('wct'), workContractRevision: 1, revision: 1, state: 'ready', title: 'Implement change', goal: 'Deliver tested change', criterionIds: [id('crt')], dependsOn: [] },
  WorkRun: { ...common, ...contract, ...identity, id: id('wrn'), workItemId: id('wit'), workItemRevision: 1, revision: 1, state: 'leased', productSessionId: id('psn'), codexThreadId: id('cdx'), candidateDigest: null },
  Candidate: { ...common, ...contract, ...run, id: id('cnd'), candidateDigest: digest, attempt: 1, candidateRef: `refs/winwincode/candidates/${'b'.repeat(40)}`, candidateCommit: 'b'.repeat(40), candidateTree: 'c'.repeat(40) },
  VerificationPlan: { ...common, ...contract, ...run, id: id('vpl'), planRevision: 1, workItemRevision: 1, candidateDigest: digest, criterionIds: [id('crt')], requiredRoles: ['verifier'], commands: ['pnpm test'], permissionProfile: 'candidate_read_only_restricted' },
  Evidence: { ...common, ...contract, ...run, id: id('evd'), verificationPlanId: id('vpl'), planRevision: 1, criterionId: id('crt'), candidateDigest: digest, producer: 'verifier', producerExecutionIdentity: identity, sourceEventId: 'event-1', sourceSequence: 1, outcome: 'succeeded' },
  Verdict: { ...common, ...contract, id: id('vdt'), workItemId: id('wit'), verificationPlanId: id('vpl'), planRevision: 1, candidateDigest: digest, status: 'inconclusive', evidenceIds: [], revision: 1 },
}

test('seven canonical domain objects accept complete values and reject missing or unknown fields', () => {
  for (const [name, value] of Object.entries(samples)) {
    assert.ok(Object.hasOwn(schema.$defs, name), `missing canonical definition: ${name}`)
    const validate = ajv.getSchema(`${schema.$id}#/$defs/${name}`)
    assert.equal(typeof validate, 'function', name)
    assert.equal(validate(value), true, `${name}: ${JSON.stringify(validate.errors)}`)
    assert.equal(validate({ ...value, stageRunId: 'old-stage' }), false, name)
    for (const field of schema.$defs[name].required) {
      const incomplete = { ...value }
      delete incomplete[field]
      assert.equal(validate(incomplete), false, `${name} requires ${field}`)
    }
  }
})

test('canonical states and fenced evidence reject invalid single-field changes', () => {
  for (const [name, change] of [
    ['WorkItem', { state: 'completed' }],
    ['WorkItem', { dependsOn: [id('wit'), id('wit')] }],
    ['WorkRun', { fencingToken: '0' }],
    ['WorkRun', { workItemId: null }],
    ['WorkRun', { id: 'wr_old' }],
    ['Candidate', { candidateCommit: 'not-a-commit' }],
    ['Candidate', { candidateCommit: 'a'.repeat(41) }],
    ['VerificationPlan', { permissionProfile: 'write' }],
    ['Evidence', { producer: 'controller' }],
    ['Evidence', { producerExecutionIdentity: { ...identity, fencingToken: '0' } }],
    ['Evidence', { sourceSequence: 0 }],
    ['Verdict', { evidenceIds: [id('evd'), id('evd')] }],
  ]) {
    const validate = ajv.getSchema(`${schema.$id}#/$defs/${name}`)
    assert.equal(validate({ ...samples[name], ...change }), false, `${name}: ${JSON.stringify(change)}`)
  }
})

test('Criterion requires machine evidence and Work graph query has one derived four-state view', () => {
  const criterion = ajv.getSchema(`${schema.$id}#/$defs/Criterion`)
  assert.equal(criterion(samples.WorkContract.criteria[0]), true)
  assert.equal(criterion({ ...samples.WorkContract.criteria[0], requiredEvidenceClass: 'model' }), false)

  const http = JSON.parse(readFileSync(new URL('../schema/winwincode/v1/control-plane-http.schema.json', import.meta.url)))
  assert.deepEqual(http.$defs.WorkGraphItemState.enum, ['ready', 'running', 'blocked', 'done'])
  assert.equal(http.$defs.WorkRunAggregateProjection.required.includes('graphItems'), true)
  assert.equal(http.$defs.WorkGraphItemProjection.properties.state.$ref, '#/$defs/WorkGraphItemState')
})

test('execution binding schema retains the canonical WorkRun identity', () => {
  const execution = JSON.parse(readFileSync(new URL('../schema/winwincode/v1/execution-port.schema.json', import.meta.url)))
  const binding = execution.$defs.SessionBindingMessage
  assert.equal(binding.properties.workRunId.$ref, './domain.schema.json#/$defs/WorkRunId')
  assert.equal(Object.hasOwn(binding.properties, 'stageRunId'), false)
})

test('dispatch requires an explicit profile and rejects caller-selected execution identities', () => {
  const http = JSON.parse(readFileSync(new URL('../schema/winwincode/v1/control-plane-http.schema.json', import.meta.url)))
  ajv.addSchema(http)
  const validate = ajv.getSchema(`${http.$id}#/$defs/DeliveryAdvancePayload`)
  const payload = { deliveryId: id('dlv'), dispatchProfile: 'executor' }
  for (const dispatchProfile of ['planner', 'executor', 'reviewer', 'verifier', 'adversarial-verifier', 'remediator']) {
    assert.equal(validate({ ...payload, dispatchProfile }), true)
  }
  assert.equal(validate({ deliveryId: id('dlv') }), false)
  assert.equal(validate({ ...payload, dispatchProfile: 'arbitrary' }), false)
  for (const key of ['workItemId', 'workRunId', 'attempt', 'leaseId', 'stageRunId']) {
    assert.equal(validate({ ...payload, [key]: 'caller-selected' }), false)
  }
})

test('Evidence artifact provenance requires the exact canonical WorkRun, never a legacy stage', () => {
  const http = JSON.parse(readFileSync(new URL('../schema/winwincode/v1/control-plane-http.schema.json', import.meta.url)))
  const validate = ajv.getSchema(`${http.$id}#/$defs/EvidenceArtifactProvenanceProjection`)
  const provenance = {
    deliveryId: id('dlv'), deliveryRevision: 1, workRunId: id('wrn'),
    sessionBindingId: 'binding:verifier', candidateRef: `git-candidate:${digest}`,
    evidenceId: id('evd'),
  }
  assert.equal(validate(provenance), true, JSON.stringify(validate.errors))
  const { workRunId, ...unbound } = provenance
  assert.equal(validate(unbound), false)
  assert.equal(validate({ ...provenance, workRunId: null }), false)
  assert.equal(validate({ ...provenance, workRunId: id('run') }), false)
  assert.equal(validate({ ...provenance, stageRunId: id('run') }), false)
  assert.equal(validate({ ...unbound, stageRunId: id('run') }), false)
})


test('diagram execution provenance binds one required WorkRun and WorkItem without old identities', () => {
  const http = JSON.parse(readFileSync(new URL('../schema/winwincode/v1/control-plane-http.schema.json', import.meta.url)))
  const validate = ajv.getSchema(`${http.$id}#/$defs/StrongFlowDiagramExecutionProvenanceProjection`)
  const provenance = { workRunId: id('wrn'), workItemId: id('wit'), sessionBindingId: 'binding:executor', evidenceRefIds: [] }
  assert.equal(validate(provenance), true, JSON.stringify(validate.errors))
  for (const key of ['workRunId', 'workItemId']) {
    const missing = { ...provenance }
    delete missing[key]
    assert.equal(validate(missing), false)
    assert.equal(validate({ ...provenance, [key]: null }), false)
  }
  assert.equal(validate({ ...provenance, stageRunId: id('run') }), false)
  assert.equal(validate({ ...provenance, deliveryTaskId: id('dtk') }), false)
})


test('WorkRun queries bootstrap a Delivery or constrain an exact item and reject old identities', () => {
  const http = JSON.parse(readFileSync(new URL('../schema/winwincode/v1/control-plane-http.schema.json', import.meta.url)))
  if (!ajv.getSchema(http.$id)) ajv.addSchema(http)
  const validate = ajv.getSchema(`${http.$id}#/$defs/WorkRunGetParameters`)
  const query = { deliveryId: id('dlv'), workItemId: null, workRunId: null, atCursor: null }
  assert.equal(validate(query), true, JSON.stringify(validate.errors))
  assert.equal(validate({ ...query, workItemId: id('wit'), workRunId: id('wrn') }), true)
  assert.equal(validate({ ...query, workRunId: id('wrn') }), true)
  for (const bad of [
    { ...query, workItemId: id('dtk') },
    { ...query, workRunId: id('run') },
    { ...query, stageRunId: id('run') },
    { deliveryId: query.deliveryId, atCursor: null },
  ]) assert.equal(validate(bad), false, JSON.stringify(bad))
})

test('retired stage approval is rejected while canonical WorkItem creation remains public', () => {
  const http = JSON.parse(readFileSync(new URL('../schema/winwincode/v1/control-plane-http.schema.json', import.meta.url)))
  if (!ajv.getSchema(http.$id)) ajv.addSchema(http)
  const validate = ajv.getSchema(`${http.$id}#/$defs/CommandRequest`)
  const command = {
    schemaVersion: 'winwincode/v1', requestId: id('req'),
    actor: { kind: 'user', id: id('usr') },
    scope: { kind: 'repository', organizationId: id('org'), workspaceId: id('wsp'), projectId: id('prj'), repositoryId: id('rep') },
    command: 'delivery.task_breakdown.create', expectedRevision: 1,
    payload: { deliveryId: id('dlv'), expectedRevision: 1, contractRevision: 1,
      items: [{ id: id('wit'), title: 'Implement', goal: 'Implement requirement', criterionIds: [id('crt')], dependsOn: [] }] },
  }
  assert.equal(validate(command), true, JSON.stringify(validate.errors))
  assert.equal(validate({ ...command, command: 'delivery.approve_task_breakdown', payload: { deliveryId: id('dlv'), reviewSetSha256: digest } }), false)
  assert.equal(Object.keys(http.$defs).some(name => name.startsWith('DeliveryApproveTaskBreakdown')), false)
})
