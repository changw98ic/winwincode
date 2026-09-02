import assert from 'node:assert/strict'
import { spawn } from 'node:child_process'
import { resolve } from 'node:path'
import test from 'node:test'

import { keylessFixtureEnvironment } from './fixtures/delivery-service-testkit.mjs'

const root = resolve(import.meta.dirname, '..')

function runScenario() {
  return new Promise((resolvePromise, rejectPromise) => {
    const child = spawn(process.execPath, [
      resolve(root, 'tests/fixtures/full-delivery-scenario.mjs'),
    ], {
      cwd: root,
      env: keylessFixtureEnvironment(),
      stdio: ['ignore', 'pipe', 'pipe'],
    })
    let stdout = ''
    let stderr = ''
    const timer = setTimeout(() => {
      child.kill('SIGKILL')
      rejectPromise(new Error(`full Delivery scenario timed out\n${stderr}\n${stdout}`))
    }, 90_000)
    child.stdout.on('data', chunk => { stdout += chunk })
    child.stderr.on('data', chunk => { stderr += chunk })
    child.on('error', error => {
      clearTimeout(timer)
      rejectPromise(error)
    })
    child.on('close', (code, signal) => {
      clearTimeout(timer)
      if (code !== 0 || signal !== null) {
        rejectPromise(new Error([
          `full Delivery scenario failed with code=${String(code)} signal=${signal ?? 'none'}`,
          stderr,
          stdout,
        ].filter(Boolean).join('\n')))
        return
      }
      const line = stdout.split('\n').find(entry => entry.trim().startsWith('{'))
      if (line === undefined) {
        rejectPromise(new Error(`full Delivery scenario returned no result\n${stderr}\n${stdout}`))
        return
      }
      try {
        resolvePromise({ result: JSON.parse(line), stderr })
      } catch (error) {
        rejectPromise(error)
      }
    })
  })
}

test('one keyless Delivery requires review, fails, reworks, reverifies, and delivers', async () => {
  const { result, stderr } = await runScenario()
  assert.equal(stderr, '')
  assert.equal(result.finalStatus, 'delivered')
  assert.notEqual(result.planReview.firstDigest, result.planReview.revisedDigest)
  assert.equal(result.planReview.supersededApprovalError, 'DELIVERY_CONFLICT')
  assert.notEqual(result.candidates.failed, result.candidates.passed)
  assert.equal(result.candidates.staleCandidateError, 'INVALID_REQUEST')
  assert.deepEqual(result.verdicts, { failed: 'fail', passed: 'pass' })
  assert.equal(result.evidenceCount > 0, true)
  assert.equal(result.stageCount, result.bindingCount)
  assert.equal(result.projectedSubagentCount >= 2, true)
  assert.equal(result.modelCalls, 2)
  assert.equal(result.measures.runKind, 'deterministic')
  assert.equal(result.measures.outcome.classification.value, 'proven-success')
  assert.equal(result.measures.dimensions.completeness.status.value, 'complete')
  assert.equal(
    result.measures.dimensions.confidence.status.value,
    'independently-supported',
  )
  assert.equal(result.measures.dimensions.stability.status.value, 'reworked')
  assert.equal(result.measures.dimensions.efficiency.modelCallCount.value, 2)
  assert.equal(result.measures.dimensions.efficiency.totalTokens.value, 63)
  assert.equal(result.measures.dimensions.efficiency.parallelExecutionObserved.value, true)
  assert.deepEqual(result.credentialNames, [])
  assert.deepEqual(result.rootEntries, ['home', 'repository'])
})
