import assert from 'node:assert/strict'
import test from 'node:test'

import { runApiProductionVertical } from '../scripts/run-api-production-vertical.mjs'

/**
 * Production vertical acceptance coverage is retained: Chat, StrongFlow,
 * cancel, restart. The path is Device Worker only.
 *
 * Live execution requires:
 * - built winwincode-server / winwincode-kernel-helper / wwc CLI
 * - Device enrollment + occupancy + repository binding
 * - Device-local Provider (deterministic test Provider or real Device GLM config)
 *
 * Server-local model execution is not a fallback. When the Device CLI or
 * Device Provider runtime is unavailable, this test fails with that exact
 * runtime gap instead of fabricating success.
 */
test('standalone Server API drives Chat and StrongFlow through Device Worker production vertical', async () => {
  const report = await runApiProductionVertical({
    devicePrerequisites: true,
  })
  assert.equal(report.schemaVersion, 'winwincode.api-production-vertical.v1')
  assert.equal(report.execution, 'device-worker-only')
  assert.ok(Array.isArray(report.devicePrerequisites))
  assert.ok(report.devicePrerequisites.includes('device-enroll-pair'))
  assert.ok(report.devicePrerequisites.includes('chat-strongflow-cancel-restart'))
  assert.equal(report.deviceProvider?.secretPlacement, 'device-local-only')
  assert.ok(report.devicePath, 'production vertical must expose Device path evidence')
  assert.ok(report.devicePath.publicClientId)
  assert.equal(report.flow.chat.status, 'Completed')
  assert.equal(report.flow.chat.assistant.role, 'assistant')
  assert.equal(report.flow.chat.assistant.state, 'completed')
  assert.ok(report.flow.chat.assistant.content.trim().length > 0)
  assert.equal(report.flow.strongflow.status, 'done')
  assert.equal(report.flow.strongflow.verdictStatus, 'pass')
  assert.ok(report.flow.strongflow.workItemStates.length > 0)
  assert.equal(report.flow.strongflow.workItemStates.every(state => state === 'done'), true)
  assert.ok(report.flow.strongflow.workRunStates.length > 0)
  assert.equal(report.flow.strongflow.workRunStates.every(state => state === 'settled'), true)
  assert.deepEqual(report.deterministic, {
    contentEqual: true,
    firstSessionId: 'psn_01J00000000000000000000001',
    repeatSessionId: 'psn_01J00000000000000000000002',
  })
  assert.deepEqual(report.restart, {
    deliveryBytesStable: true,
    messageBytesStable: true,
    status: 'done',
  })
})
