import assert from 'node:assert/strict'
import { access } from 'node:fs/promises'

import {
  DeliveryServiceFixtureTestkit,
  ScriptedDshFixtureRuntime,
} from './delivery-service-testkit.mjs'

assert.equal(typeof globalThis.gc, 'function')

const iterations = Number(process.env.WINWINCODE_CLEANUP_STRESS_ITERATIONS ?? '1')
assert.equal(Number.isSafeInteger(iterations) && iterations >= 1 && iterations <= 32, true)

for (let index = 0; index < iterations; index += 1) {
  let kit = await DeliveryServiceFixtureTestkit.create({
    deliveryId: `delivery-testkit-cleanup-${String(index)}`,
  })
  const root = kit.root
  let runtime = await ScriptedDshFixtureRuntime.create({
    owner: kit,
    home: kit.home,
    workspace: kit.repository,
    script: [{
      text: `cleanup iteration ${String(index)}`,
      usage: { inputTokens: 2, outputTokens: 2 },
    }],
  })
  await runtime.runRole({
    sessionId: `dsh-testkit-cleanup-${String(index)}`,
    roleId: 'requirements',
    prompt: 'Exercise deterministic fixture cleanup.',
    maxTokens: 32,
  })

  await kit.cleanup()
  await assert.rejects(access(root), error => error?.code === 'ENOENT')
  runtime = null
  kit = null
  for (let collection = 0; collection < 8; collection += 1) {
    globalThis.gc()
    await new Promise(resolve => setTimeout(resolve, 10))
  }
}

process.stdout.write(`${JSON.stringify({ iterations, status: 'clean' })}\n`)
