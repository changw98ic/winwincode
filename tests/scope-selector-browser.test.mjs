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

test('real Chrome cascades, switches, restores, and revokes one exact Scope', async t => {
  const chromePath = chromeBinary()
  assert.notEqual(chromePath, null, 'Chrome or Chromium is required for Scope validation')
  command(root, 'corepack', ['pnpm', '--filter', '@winwincode/client', 'build'])
  const directory = mkdtempSync(join(tmpdir(), 'winwincode-scope-selector-'))
  const certificateFiles = certificate(root, directory)
  const clientServer = staticClientServer({
    root,
    certificateFiles,
    fixturePath: 'tests/fixtures/browser-scope-selector.mjs',
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
  await devtools.send('Page.navigate', {
    url: `https://client.localhost:${String(clientPort)}/#/settings`,
  }, sessionId)
  await waitForGlobal(devtools, sessionId, 'scopeSelectorReady')

  const selected = await evaluate(devtools, sessionId, 'globalThis.runScopeSelection()')
  assert.equal(selected.initialProductReads, 0)
  assert.deepEqual(selected.initial.labels, ['Organization', 'Workspace', 'Project', 'Repository'])
  assert.deepEqual(selected.initial.disabled, {
    organization: false,
    workspace: true,
    project: true,
    repository: true,
  })
  assert.match(selected.initial.accessText, /Choose a Scope/iu)
  assert.match(selected.hash, /^#\/settings\?organizationId=.*&workspaceId=.*&projectId=.*&repositoryId=/u)
  assert.equal(selected.selected.values.repository, 'rep_00000000000000000000000002')
  assert.equal(selected.selectedProductScopes.every(scope => (
    scope.repositoryId === 'rep_00000000000000000000000002'
  )), true)

  const switched = await evaluate(
    devtools,
    sessionId,
    'globalThis.switchScopeWithNetworkFailure()',
  )
  assert.equal(switched.oldSubscriptionClosed, true)
  assert.equal(switched.featureVisible, false)
  assert.equal(switched.state.retryVisible, true)
  assert.match(switched.state.status, /network/iu)

  await evaluate(devtools, sessionId, 'globalThis.restoreSecondRepository()')
  await devtools.send('Page.reload', {}, sessionId)
  await waitForGlobal(devtools, sessionId, 'scopeSelectorReady')
  const restored = await evaluate(devtools, sessionId, 'globalThis.inspectRestoredScope()')
  assert.equal(restored.selected.values.repository, 'rep_00000000000000000000000002')
  assert.equal(restored.settingsScope.repositoryId, 'rep_00000000000000000000000002')

  const revoked = await evaluate(devtools, sessionId, 'globalThis.revokeRestoredScope()')
  assert.equal(revoked.oldSubscriptionClosed, true)
  assert.equal(revoked.afterProductReads, revoked.beforeProductReads)
  assert.equal(revoked.state.accessRole, 'alert')
  assert.match(revoked.state.accessText, /no longer authorized/iu)
})
