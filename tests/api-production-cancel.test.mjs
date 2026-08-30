import assert from 'node:assert/strict'
import test from 'node:test'

import { runApiProductionVertical } from '../scripts/run-api-production-vertical.mjs'

test('standalone Server API exposes health, cancellation, terminal Chat, and restart evidence', async () => {
  const report = await runApiProductionVertical({
    build: false,
    includeStrongFlow: false,
    repeat: false,
    timeoutMillis: 60_000,
  })
  assert.deepEqual(report.health, {
    initial: 'ready',
    afterRestart: 'ready',
  })
  assert.equal(report.flow.chat.status, 'Completed')
  assert.equal(report.flow.chat.assistant.state, 'completed')
  assert.deepEqual(report.flow.cancel, {
    providerRoute: {
      providerId: 'winwincode-loopback',
      modelId: 'loopback-model',
      credentialReferenceId: 'crd_01J00000000000000000000001',
    },
    revision: 2,
    state: 'cancelled',
  })
  assert.deepEqual(report.restart, {
    deliveryBytesStable: null,
    messageBytesStable: true,
    status: 'cancelled',
  })
})
