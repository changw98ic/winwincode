import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'
import { resolve } from 'node:path'
import test from 'node:test'

import {
  buildLegacyDeliveryStrongFlowOracle,
  normalizeLegacyDeliveryOracleValue,
} from './fixtures/delivery-strongflow-differential-oracle.mjs'

let fullOraclePromise

function fullOracle() {
  fullOraclePromise ??= buildLegacyDeliveryStrongFlowOracle()
  return fullOraclePromise
}

test('legacy Delivery oracle normalization removes only local execution facts', () => {
  const input = {
    error: { code: 'REVISION_CONFLICT', currentRevision: 7 },
    snapshot: { revision: 7, status: 'needs-attention' },
    event: { kind: 'command.finished', command: [process.execPath, '--test'] },
    verdict: { status: 'inconclusive' },
    repository: '/private/tmp/oracle-random/repository',
    proof: 'fixture-local-session-proof-value',
  }

  assert.deepEqual(normalizeLegacyDeliveryOracleValue(input, {
    root: '/private/tmp/oracle-random',
  }), {
    error: { code: 'REVISION_CONFLICT', currentRevision: 7 },
    event: { command: ['<NODE_EXECUTABLE>', '--test'], kind: 'command.finished' },
    proof: '<AUTH_PROOF>',
    repository: '<ORACLE_ROOT>/repository',
    snapshot: { revision: 7, status: 'needs-attention' },
    verdict: { status: 'inconclusive' },
  })
})

test('legacy Delivery oracle records requestId replay through the public invoker', async () => {
  const oracle = await buildLegacyDeliveryStrongFlowOracle({
    scenarioIds: ['request-id-replay'],
  })

  assert.equal(oracle.schemaVersion, 'winwincode.delivery-strongflow-differential-oracle.v1')
  assert.equal(oracle.scenarios.length, 1)
  const [scenario] = oracle.scenarios
  assert.equal(scenario.id, 'request-id-replay')
  assert.deepEqual(
    scenario.commands.filter(command => command.kind === 'strongflow.request')
      .map(command => [command.request.operation, command.response.ok]),
    [
      ['createDelivery', true],
      ['createDelivery', true],
      ['getDeliveryProjection', true],
    ],
  )
  assert.equal(scenario.observation.store.records.length, 1)
  assert.deepEqual(
    scenario.commands[0].response.result.delivery,
    scenario.commands[1].response.result.delivery,
  )
  assert.equal(scenario.observation.snapshot.revision, 1)
  assert.deepEqual(scenario.observation.events, [])
  assert.equal(scenario.observation.verdict, null)
})

test('legacy Delivery oracle covers every Rust differential scenario with full observations', async () => {
  const oracle = await fullOracle()
  assert.deepEqual(oracle.scenarios.map(scenario => scenario.id), [
    'success-closed-loop',
    'request-id-replay',
    'revision-conflict',
    'corruption-recovery',
    'task-dag',
    'candidate-invalidation',
    'attention',
    'inconclusive',
    'infra-error',
    'rework',
  ])

  for (const scenario of oracle.scenarios) {
    assert.equal(scenario.commands.length > 0, true, scenario.id)
    assert.deepEqual(Object.keys(scenario.observation), [
      'events',
      'projection',
      'snapshot',
      'store',
      'verdict',
    ])
    assert.equal(typeof scenario.observation.snapshot.revision, 'number', scenario.id)
    assert.equal(Array.isArray(scenario.observation.events), true, scenario.id)
  }

  const byId = Object.fromEntries(oracle.scenarios.map(scenario => [scenario.id, scenario]))
  assert.equal(byId['success-closed-loop'].observation.snapshot.status, 'delivered')
  assert.equal(byId['success-closed-loop'].observation.verdict.status, 'pass')
  assert.deepEqual(byId['revision-conflict'].assertions, {
    currentRevision: 2,
    errorCode: 'REVISION_CONFLICT',
    snapshotUnchanged: true,
  })
  assert.deepEqual(byId['corruption-recovery'].assertions, {
    corruptedReadError: 'STORE_FAILURE',
    restoredSnapshotEqual: true,
  })
  assert.deepEqual(byId['task-dag'].assertions, {
    blockedTaskError: 'WRONG_DELIVERY_STATE',
    cycleError: 'INVALID_REQUEST',
    durableTaskOrder: ['oracle-task-prerequisite', 'oracle-task-dependent'],
  })
  assert.equal(byId['candidate-invalidation'].assertions.staleCandidateError, 'INVALID_REQUEST')
  assert.equal(byId.attention.assertions.openAttentionStatus, 'needs-attention')
  assert.equal(byId.attention.assertions.resolvedStatus, 'executing')
  assert.equal(byId.inconclusive.observation.verdict.status, 'inconclusive')
  assert.equal(byId['infra-error'].observation.verdict.status, 'infra_error')
  assert.deepEqual(byId.rework.assertions.verdicts, ['fail', 'pass'])
  assert.equal(byId.rework.assertions.enteredRework, true)
  assert.equal(byId.rework.assertions.candidateChanged, true)

  for (const id of [
    'success-closed-loop',
    'candidate-invalidation',
    'inconclusive',
    'infra-error',
    'rework',
  ]) {
    assert.equal(byId[id].observation.events.length > 0, true, id)
    assert.notEqual(byId[id].observation.projection.runtimeExecution, null, id)
  }
})

test('committed legacy Delivery oracle is current, portable, and contains no credential value', async () => {
  const oracle = await fullOracle()
  const expected = JSON.parse(await readFile(resolve(
    import.meta.dirname,
    'fixtures',
    'oracles',
    'delivery-strongflow-typescript.v1.json',
  ), 'utf8'))
  assert.deepEqual(oracle, expected)

  const serialized = JSON.stringify(oracle)
  assert.equal(serialized.includes(process.execPath), false)
  assert.equal(serialized.includes('fixture-local-session-proof-value'), false)
  assert.equal(serialized.includes('fixture-local-peer-proof-value'), false)
  assert.doesNotMatch(serialized, /\/(?:Users|Volumes|private\/tmp)\//u)
  assert.match(serialized, /<NODE_EXECUTABLE>/u)
  assert.match(serialized, /<AUTH_PROOF>/u)
  assert.match(serialized, /<ORACLE_ROOT>/u)
})
