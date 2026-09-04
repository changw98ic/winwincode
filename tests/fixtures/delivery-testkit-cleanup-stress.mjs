import assert from 'node:assert/strict'
import { access } from 'node:fs/promises'

import { DeliveryServiceFixtureTestkit } from './delivery-service-testkit.mjs'

assert.equal(typeof globalThis.gc, 'function')

const iterations = Number(process.env.WINWINCODE_CLEANUP_STRESS_ITERATIONS ?? '1')
assert.equal(Number.isSafeInteger(iterations) && iterations >= 1 && iterations <= 32, true)

for (let index = 0; index < iterations; index += 1) {
  let kit = await DeliveryServiceFixtureTestkit.create({
    deliveryId: `dlv_${String(index).padStart(26, '0')}`,
  })
  const root = kit.root
  await kit.preparePlanReview()

  await kit.cleanup()
  await assert.rejects(access(root), error => error?.code === 'ENOENT')
  kit = null
  for (let collection = 0; collection < 8; collection += 1) {
    globalThis.gc()
    await new Promise(resolve => setTimeout(resolve, 10))
  }
}

process.stdout.write(`${JSON.stringify({ iterations, status: 'clean' })}\n`)
