import assert from 'node:assert/strict'
import test from 'node:test'

import { runApiProductionVertical } from '../scripts/run-api-production-vertical.mjs'

test('standalone Server API drives Chat and StrongFlow through the production vertical', async () => {
  const report = await runApiProductionVertical()
  assert.equal(report.schemaVersion, 'winwincode.api-production-vertical.v1')
  assert.equal(report.flow.chat.status, 'Completed')
  assert.equal(report.flow.chat.assistant.role, 'assistant')
  assert.equal(report.flow.chat.assistant.state, 'completed')
  assert.ok(report.flow.chat.assistant.content.trim().length > 0)
  assert.equal(report.flow.strongflow.status, 'delivered')
  assert.equal(report.flow.strongflow.verdictStatus, 'pass')
  assert.ok(report.flow.strongflow.taskStatuses.length > 0)
  assert.equal(report.flow.strongflow.taskStatuses.every(status => status === 'completed'), true)
  for (const role of ['planner', 'executor', 'reviewer', 'verifier']) {
    assert.ok(report.flow.strongflow.stageRoles.includes(role), `missing ${role} stage`)
  }
  assert.deepEqual(report.deterministic, {
    contentEqual: true,
    firstSessionId: 'psn_01J00000000000000000000001',
    repeatSessionId: 'psn_01J00000000000000000000002',
  })
  assert.deepEqual(report.restart, {
    deliveryBytesStable: true,
    messageBytesStable: true,
    status: 'delivered',
  })
})
