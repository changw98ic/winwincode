import assert from 'node:assert/strict'
import { mkdtempSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import test from 'node:test'

import {
  certificate,
  chromeBinary,
  closeServer,
  command,
  DevTools,
  evaluate,
  freePort,
  listen,
  staticClientServer,
  stopChild,
  waitForGlobal,
} from './fixtures/real-browser-harness.mjs'

const root = resolve(import.meta.dirname, '..')

test('real browser preserves drafts and gives safe recovery diagnostics across connection failures', async t => {
  const chromePath = chromeBinary()
  assert.notEqual(chromePath, null, 'Chrome or Chromium is required for the Client reliability test')
  command(root, 'corepack', ['pnpm', '--filter', '@winwincode/client', 'build'])
  const directory = mkdtempSync(join(tmpdir(), 'winwincode-client-reliability-'))
  const certificateFiles = certificate(root, directory)
  const clientServer = staticClientServer({
    root,
    certificateFiles,
    fixturePath: 'tests/fixtures/browser-client-feature-routes.mjs',
    configuration: () => ({}),
  })
  const clientPort = await listen(clientServer)
  let chrome = null
  let devtools = null
  t.after(async () => {
    devtools?.close()
    await Promise.all([
      ...(chrome === null ? [] : [stopChild(chrome, 'SIGTERM')]),
      closeServer(clientServer),
    ])
    rmSync(directory, { recursive: true, force: true })
  })

  const launched = await DevTools.launch({
    chromePath,
    directory,
    debugPort: await freePort(),
  })
  chrome = launched.chrome
  devtools = launched.devtools
  const { targetId } = await devtools.send('Target.createTarget', { url: 'about:blank' })
  const { sessionId } = await devtools.send('Target.attachToTarget', {
    targetId,
    flatten: true,
  })
  await devtools.send('Runtime.enable', {}, sessionId)
  await devtools.send('Page.enable', {}, sessionId)
  await devtools.send('Emulation.setDeviceMetricsOverride', {
    width: 360,
    height: 800,
    deviceScaleFactor: 1,
    mobile: false,
  }, sessionId)
  await devtools.send('Page.navigate', {
    url: `https://client.localhost:${String(clientPort)}/#/settings`,
  }, sessionId)
  await waitForGlobal(devtools, sessionId, 'runReliabilityScenario')
  const result = await evaluate(devtools, sessionId, 'globalThis.runReliabilityScenario()')

  assert.equal(result.connected.status, 'connected')
  assert.equal(result.offline.status, 'offline')
  assert.equal(result.offlineDraft, 'draft-provider')
  assert.equal(result.authenticationDraft, 'draft-provider')
  assert.match(result.authenticationDiagnostic, /connection=authentication-required/iu)
  assert.match(result.authenticationDiagnostic, /scope=repository:org …000001/iu)
  assert.doesNotMatch(result.authenticationDiagnostic, /SECRET_TOKEN|private\/repository/iu)
  assert.equal(result.reconnecting.status, 'reconnecting')
  assert.equal(result.reconnected.status, 'connected')
  assert.equal(result.reconnectCount > 0, true)
  assert.equal(result.network.status, 'reconnecting')
  assert.equal(result.permission.status, 'permission-denied')
  assert.equal(result.version.status, 'version-mismatch')
  assert.equal(result.authentication.status, 'authentication-required')
  assert.match(result.boundaryText, /This area stopped unexpectedly/iu)
  assert.doesNotMatch(result.boundaryText, /SECRET_TOKEN|private\/repository|raw render payload/iu)
  assert.match(result.copied, /connection=refresh-required/iu)
  assert.match(result.copied, /scope=repository:org …000001/iu)
  assert.doesNotMatch(result.copied, /SECRET_TOKEN|private\/repository|raw render payload/iu)
  assert.equal(result.copyFeedback, 'Diagnostic summary copied.')
  assert.deepEqual(result.focus, {
    active: true,
    outlineStyle: 'solid',
    outlineWidth: '3px',
  })
  assert.equal(result.safeHash, '#/chat')
})
