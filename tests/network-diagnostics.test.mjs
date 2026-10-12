// SPDX-License-Identifier: Apache-2.0
import assert from 'node:assert/strict'
import test from 'node:test'
import { pathToFileURL } from 'node:url'

const executorUrl = process.env.WWC_NETWORK_DIAGNOSTICS_MODULE
  ? pathToFileURL(process.env.WWC_NETWORK_DIAGNOSTICS_MODULE)
  : new URL('../packages/network-request/src/index.mjs', import.meta.url)
const { classifyError, executeRequest, NetworkError, httpFailure } = await import(executorUrl)

test('every failed network attempt exposes a safe underlying diagnostic', async () => {
  const sensitiveText = 'SYNTHETIC_PRIVATE_PAYLOAD https://invalid.example/private?token=SYNTHETIC_CREDENTIAL'
  const codes = ['ECONNREFUSED', 'ECONNRESET', 'ETIMEDOUT', 'ENOTFOUND']
  const errors = codes.map(code => {
    const cause = Object.assign(new Error(sensitiveText), { code })
    return new TypeError(sensitiveText, { cause })
  })
  const attempts = []
  let sent = 0
  await assert.rejects(executeRequest(async () => { throw errors[sent++] }, {
    replay: 'retry_inference', maxAttempts: 4, jitter: () => 0,
    waitBeforeRetry: async () => {}, onAttempt: fact => attempts.push(fact),
  }))
  assert.equal(sent, 4)
  assert.deepEqual(attempts.map(fact => fact.networkAttempt), [1, 2, 3, 4])
  for (const fact of attempts) {
    assert.ok(fact.failure.diagnostic, 'each retained attempt needs a payload-free diagnostic')
    assert.equal(typeof fact.failure.diagnostic.code, 'string')
  }
  const retained = JSON.stringify(attempts)
  assert.equal(retained.includes('SYNTHETIC_PRIVATE_PAYLOAD'), false)
  assert.equal(retained.includes('SYNTHETIC_CREDENTIAL'), false)
  assert.equal(retained.includes('invalid.example'), false)
})

test('retry exhaustion exposes all safe attempt facts on the final error', async () => {
  const attempts = []
  let sent = 0, finalError
  try {
    await executeRequest(async () => {
      sent += 1
      throw new TypeError('SYNTHETIC_PRIVATE_PAYLOAD', {
        cause: Object.assign(new Error('SYNTHETIC_PRIVATE_CAUSE'), { code: 'ECONNRESET' }),
      })
    }, {
      replay: 'retry_inference', maxAttempts: 4, jitter: () => 0,
      waitBeforeRetry: async () => {}, onAttempt: fact => attempts.push(fact),
    })
  } catch (error) { finalError = error }
  assert.equal(sent, 4)
  assert.ok(finalError)
  assert.ok(Array.isArray(finalError.networkAttempts), 'final error must carry safe attempt history')
  assert.deepEqual(finalError.networkAttempts.map(fact => fact.networkAttempt), [1, 2, 3, 4])
  assert.deepEqual(finalError.networkAttempts.map(fact => fact.attempt), [1, 2, 3, 4])
  assert.ok(finalError.networkAttempts.every(fact => Number.isInteger(fact.connectionWaits)))
  assert.ok(finalError.networkAttempts.every(fact => fact.failure.diagnostic))
  assert.ok(finalError.cause, 'original cause remains available to in-process callers')
  const retained = JSON.stringify(finalError.networkAttempts)
  assert.equal(retained.includes('SYNTHETIC_PRIVATE'), false)
})

test('authority expiry retains safe attempt evidence without serializing the original error', async () => {
  let active = true, finalError
  try {
    await executeRequest(async () => {
      active = false
      throw new TypeError('SYNTHETIC_PRIVATE_PAYLOAD', {
        cause: Object.assign(new Error('SYNTHETIC_PRIVATE_CAUSE'), { code: 'ECONNRESET' }),
      })
    }, { replay: 'retry_inference', canStart: () => active })
  } catch (error) { finalError = error }
  assert.ok(Array.isArray(finalError.networkAttempts))
  assert.equal(finalError.networkStopReason, 'authority_ended')
  assert.equal(finalError.networkAttempts.length, 1)
  assert.ok(finalError.networkAttempts[0].failure.diagnostic)
  assert.equal(JSON.stringify(finalError.networkAttempts).includes('SYNTHETIC_PRIVATE'), false)
})

test('caller supplied failure details cannot inject payload fields into safe diagnostics', () => {
  const failure = classifyError({
    networkFailure: {
      kind: 'transport_interrupted', acceptance: 'unknown', phase: 'response_headers',
      httpStatus: null, retryAfterMs: null,
      diagnostic: {
        code: 'io', ioKind: 'connection_reset', message: 'SYNTHETIC_PRIVATE_PAYLOAD',
        url: 'https://invalid.example/?token=SYNTHETIC_CREDENTIAL',
      },
    },
  })
  assert.ok(failure.diagnostic)
  const allowed = new Set(['code', 'ioKind', 'osCode', 'line', 'column'])
  assert.ok(Object.keys(failure.diagnostic).every(key => allowed.has(key)))
  assert.equal(JSON.stringify(failure).includes('SYNTHETIC_PRIVATE'), false)
  assert.equal(JSON.stringify(failure).includes('invalid.example'), false)
})

test('nested client wrappers preserve the inner safe NetworkError failure', () => {
  const inner = new NetworkError(httpFailure(503, 2000))
  const middle = Object.assign(new Error('SYNTHETIC_PRIVATE_MIDDLE', { cause: inner }), { code: 'NETWORK_ERROR' })
  const outer = Object.assign(new Error('SYNTHETIC_PRIVATE_OUTER', { cause: middle }), { code: 'NETWORK_ERROR' })
  const classified = classifyError(outer)
  assert.deepEqual(classified, inner.failure)
  assert.equal(classified.httpStatus, 503)
  assert.equal(classified.diagnostic.code, 'http_status')
  assert.equal(JSON.stringify(classified).includes('SYNTHETIC_PRIVATE'), false)
})

test('beforeAttempt persistence failure stops further sends and retains previous evidence', async () => {
  let sends = 0, finalError
  const storageFailure = new Error('SYNTHETIC_PRIVATE_STORAGE_FAILURE')
  try {
    await executeRequest(async () => { sends += 1; throw new NetworkError(httpFailure(503)) }, {
      replay: 'retry_inference', maxAttempts: 4, waitBeforeRetry: async () => {},
      beforeAttempt: fact => { if (fact.networkAttempt === 2) throw storageFailure },
    })
  } catch (error) { finalError = error }
  assert.equal(sends, 1)
  assert.equal(finalError.networkStopReason, 'storage_failed')
  assert.equal(finalError.networkAttempts.length, 1)
  assert.equal(finalError.networkAttempts[0].failure.httpStatus, 503)
  assert.equal(JSON.stringify(finalError.networkAttempts).includes('SYNTHETIC_PRIVATE'), false)
})

test('onAttempt persistence failure fences retry and retains the failed attempt', async () => {
  let sends = 0, finalError
  try {
    await executeRequest(async () => { sends += 1; throw new NetworkError(httpFailure(503)) }, {
      replay: 'retry_inference', maxAttempts: 4,
      onAttempt: () => { throw new Error('SYNTHETIC_PRIVATE_STORAGE_FAILURE') },
    })
  } catch (error) { finalError = error }
  assert.equal(sends, 1)
  assert.equal(finalError.networkStopReason, 'storage_failed')
  assert.equal(finalError.networkAttempts.length, 1)
  assert.equal(finalError.networkAttempts[0].failure.httpStatus, 503)
})

test('success receipt persistence failure retains earlier retry evidence', async () => {
  let sends = 0, finalError
  try {
    await executeRequest(async () => {
      sends += 1
      if (sends === 1) throw new NetworkError(httpFailure(503))
      return 'safe success result'
    }, {
      replay: 'retry_inference', maxAttempts: 4, waitBeforeRetry: async () => {},
      onAttempt: fact => { if (fact.outcome === 'succeeded') throw new Error('SYNTHETIC_PRIVATE_STORAGE_FAILURE') },
    })
  } catch (error) { finalError = error }
  assert.equal(sends, 2)
  assert.equal(finalError.networkStopReason, 'storage_failed')
  assert.equal(finalError.networkAttempts.length, 2)
  assert.deepEqual(finalError.networkAttempts.map(fact => fact.outcome), ['failed', 'succeeded'])
  assert.equal(finalError.networkAttempts[0].failure.httpStatus, 503)
})
