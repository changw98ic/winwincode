import assert from 'node:assert/strict'
import test from 'node:test'

import {
  materializeStrongFlowDeliveryAdvanceFailure,
  materializeStrongFlowDeliveryAdvanceRequest,
  parseStrongFlowDeliveryAdvanceRequest,
  parseStrongFlowDeliveryAdvanceResponse,
} from '../packages/contracts/dist/index.js'

test('StrongFlow stage-advance boundary rejects extra fields and materializes typed failures', () => {
  const request = materializeStrongFlowDeliveryAdvanceRequest(
    'advance-contract-1',
    'dlv_0Y8J5DC68YS0Y0MHYQY554KBM2',
    3,
  )
  assert.deepEqual(request, {
    schemaVersion: 1,
    requestId: 'advance-contract-1',
    deliveryId: 'dlv_0Y8J5DC68YS0Y0MHYQY554KBM2',
    expectedRevision: 3,
  })
  assert.throws(
    () => parseStrongFlowDeliveryAdvanceRequest({ ...request, operation: 'startStage' }),
    /unexpected shape/u,
  )

  const failure = materializeStrongFlowDeliveryAdvanceFailure({
    requestId: request.requestId,
    code: 'REVISION_CONFLICT',
    message: 'Delivery changed before this stage could start.',
    currentRevision: 4,
  })
  assert.deepEqual(parseStrongFlowDeliveryAdvanceResponse(failure), failure)
  assert.equal(Object.isFrozen(failure), true)
})
