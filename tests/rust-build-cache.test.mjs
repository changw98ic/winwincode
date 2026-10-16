import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import test from 'node:test'

import { summarizeWriteErrors } from '../scripts/report-sccache-errors.mjs'

test('managed Rust build cache expiration, locks, failure and path safety', () => {
  const result = spawnSync('python3', [new URL('rust_build_cache_test.py', import.meta.url).pathname], { encoding: 'utf8' })
  assert.equal(result.status, 0, `${result.stdout}${result.stderr}`)
})

test('sccache diagnostics expose only classified backend facts', () => {
  const records = summarizeWriteErrors('DEBUG Error executing cache write: Unexpected status: 429 rate limit https://private.example/?sig=secret\nDEBUG request token=secret\nDEBUG Error executing cache write: AlreadyExists cache already exists key=private')
  assert.deepEqual(records, [
    { reason: 'service-rate-limit', status: '429', kind: 'Unexpected', count: 1 },
    { reason: 'entry-already-exists', status: null, kind: 'AlreadyExists', count: 1 },
  ])
  assert.doesNotMatch(JSON.stringify(records), /secret|private/u)
})
