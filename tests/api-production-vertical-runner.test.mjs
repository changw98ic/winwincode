import assert from 'node:assert/strict'
import {
  chmodSync,
  mkdtempSync,
  mkdirSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from 'node:fs'
import { resolve } from 'node:path'
import { tmpdir } from 'node:os'
import test from 'node:test'

import { verifyApiProductionSourceSeal } from '../scripts/run-api-production-vertical.mjs'

const root = resolve(import.meta.dirname, '..')
const runnerPath = resolve(root, 'scripts/run-api-production-vertical.mjs')
const browserGatePath = resolve(root, 'tests/browser-chat-strongflow-production.test.mjs')

test('API production vertical is a direct generated HTTP runner', async () => {
  const source = readFileSync(runnerPath, 'utf8')
  for (const endpoint of ['/api/v1/auth/session', '/api/v1/commands', '/api/v1/queries']) {
    assert.equal(source.includes(endpoint), true, `runner must call ${endpoint}`)
  }
  for (const operation of [
    'session.create',
    'session.cancel',
    'chat.submit',
    'delivery.create',
    'delivery.advance',
    'delivery.resolve_attention',
    'delivery.approve_task_breakdown',
    'delivery.submit_verdict',
    'delivery.get',
    'runtime.projection.get',
  ]) {
    assert.equal(source.includes(operation), true, `runner must cover ${operation}`)
  }
  assert.match(source, /\/health/u)
  assert.match(source, /httpsRequest/u)
  assert.match(source, /CARGO_TARGET_DIR/u)
  assert.match(source, /deliveryFailureSummary/u)
  assert.match(source, /stageRuns/u)
  assert.match(source, /providerRoute/u)
  assert.match(source, /candidateArtifact/u)
  assert.match(source, /GIT_CONFIG_NOSYSTEM/u)
  assert.match(source, /API_SOURCE_SEAL_NAME/u)
  assert.match(source, /apiProductionSourceDigest/u)
  assert.match(source, /writeApiProductionSourceSeal/u)
  assert.match(source, /verifyApiProductionSourceSeal/u)
  assert.match(source, /trackedDiffSha256/u)
  assert.match(source, /helperReleaseManifestMode/u)
  assert.match(source, /source seal missing or invalid/u)
  assert.match(source, /export async function runApiProductionVertical/u)
  assert.doesNotMatch(source, /\b(?:chromium|devtools|document|window|WebSocket)\b/iu)
})

test('skip-build rejects an old target before any API process starts', () => {
  const target = mkdtempSync(resolve(tmpdir(), 'winwincode-api-source-seal-test-'))
  const debug = resolve(target, 'debug')
  mkdirSync(debug, { recursive: true })
  const serverBinary = resolve(debug, 'winwincode-server')
  const helperExecutable = resolve(debug, 'winwincode-kernel-helper')
  writeFileSync(serverBinary, '#!/bin/sh\nexit 0\n')
  writeFileSync(helperExecutable, '#!/bin/sh\nexit 0\n')
  chmodSync(serverBinary, 0o755)
  chmodSync(helperExecutable, 0o755)
  try {
    assert.throws(
      () => verifyApiProductionSourceSeal({
        root,
        serverBinary,
        helperExecutable,
      }),
      /source seal missing or invalid/u,
    )
  } finally {
    rmSync(target, { recursive: true, force: true })
  }
})

test('browser skip-build verifies the production source seal before replacing artifacts', () => {
  const source = readFileSync(browserGatePath, 'utf8')
  const verification = source.indexOf('verifyApiProductionSourceSeal({')
  const temporaryDirectory = source.indexOf("mkdtempSync(join(tmpdir(), 'winwincode-browser-product-'))")
  const artifactReplacement = source.indexOf('rmSync(artifactDirectory, { recursive: true, force: true })')
  assert.notEqual(verification, -1, 'browser gate must verify the API production source seal')
  assert.ok(verification < temporaryDirectory, 'source verification must precede temporary resources')
  assert.ok(verification < artifactReplacement, 'source verification must preserve prior artifacts on failure')
})
