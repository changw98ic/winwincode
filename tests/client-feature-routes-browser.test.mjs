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
const productSessionId = 'psn_00000000000000000000000001'

test('real browser routes mount Settings, Approvals, and Local Operations without empty slots', async t => {
  const chromePath = chromeBinary()
  assert.notEqual(chromePath, null, 'Chrome or Chromium is required for the Client route browser test')
  command(root, 'corepack', ['pnpm', '--filter', '@winwincode/client', 'build'])
  const directory = mkdtempSync(join(tmpdir(), 'winwincode-feature-routes-'))
  const certificateFiles = certificate(root, directory)
  const clientServer = staticClientServer({
    root,
    certificateFiles,
    fixturePath: 'tests/fixtures/browser-client-feature-routes.mjs',
    configuration: () => ({}),
  })
  const clientPort = await listen(clientServer)
  const clientOrigin = `https://client.localhost:${String(clientPort)}`
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

  async function open(path) {
    await devtools.send('Page.navigate', { url: `${clientOrigin}/${path}` }, sessionId)
    await waitForGlobal(devtools, sessionId, 'inspectFeatureRoute')
  }

  await open('#/settings')
  const navigation = await evaluate(
    devtools,
    sessionId,
    'globalThis.runFeatureNavigationScenario()',
  )
  assert.equal(navigation.settings.hash, '#/settings')
  assert.match(navigation.settings.status, /^Ready/iu)
  assert.match(navigation.operations.hash, /^#\/settings\/runtime/iu)
  assert.match(navigation.operations.status, /^Ready/iu)
  assert.equal(navigation.settingsSubscriptionClosed, true)
  assert.match(navigation.unconfigured, /session/iu)
  assert.match(navigation.denied, /do not have access/iu)
  assert.doesNotMatch(navigation.denied, /private route fixture/iu)
  assert.match(navigation.network, /could not be reached/iu)
  assert.doesNotMatch(navigation.network, /private route fixture/iu)
  assert.deepEqual(navigation.calls.abortedQueries, ['settings.get'])
  assert.match(navigation.afterCancellation.status, /^Ready/iu)

  await open('#/settings')
  const desktopSettings = await evaluate(
    devtools,
    sessionId,
    'globalThis.inspectManagementPresentation("settings")',
  )
  assert.equal(desktopSettings.page, 'management')
  assert.equal(desktopSettings.panelCount, 3)
  assert.equal(desktopSettings.emptyCount, 1)
  assert.notEqual(desktopSettings.statusIcon, '')
  assert.equal(desktopSettings.statusIconHidden, 'true')
  assert.equal(desktopSettings.statusRole, 'status')
  assert.equal(desktopSettings.noHorizontalOverflow, true)
  assert.equal(desktopSettings.panelWithinPage, true)
  const focus = await evaluate(
    devtools,
    sessionId,
    'globalThis.inspectManagementFocus(".wwc-settings-local-operations-link")',
  )
  assert.equal(focus.active, true)
  assert.equal(focus.outlineStyle, 'solid')
  assert.equal(focus.outlineWidth, '3px')

  await devtools.send('Emulation.setDeviceMetricsOverride', {
    width: 360,
    height: 800,
    deviceScaleFactor: 1,
    mobile: false,
  }, sessionId)
  const compactSettings = await evaluate(
    devtools,
    sessionId,
    'globalThis.inspectManagementPresentation("settings")',
  )
  assert.equal(compactSettings.noHorizontalOverflow, true)
  assert.equal(compactSettings.panelWithinPage, true)

  await open(`#/approvals?session=${productSessionId}`)
  const approvals = await evaluate(
    devtools,
    sessionId,
    'globalThis.inspectFeatureRoute("approvals")',
  )
  assert.equal(approvals.hash, `#/approvals?session=${productSessionId}`)
  assert.match(approvals.status, /^Ready/iu)
  const compactApprovals = await evaluate(
    devtools,
    sessionId,
    'globalThis.inspectManagementPresentation("approvals")',
  )
  assert.equal(compactApprovals.panelCount, 3)
  assert.equal(compactApprovals.emptyCount, 3)
  assert.equal(compactApprovals.noHorizontalOverflow, true)

  await open('#/settings/runtime')
  const directOperations = await evaluate(
    devtools,
    sessionId,
    'globalThis.inspectFeatureRoute("operations")',
  )
  assert.equal(directOperations.hash, '#/settings/runtime')
  await devtools.send('Page.reload', {}, sessionId)
  await waitForGlobal(devtools, sessionId, 'inspectFeatureRoute')
  const restoredOperations = await evaluate(
    devtools,
    sessionId,
    'globalThis.inspectFeatureRoute("operations")',
  )
  assert.equal(restoredOperations.hash, '#/settings/runtime')
  assert.match(restoredOperations.status, /^Ready/iu)
  const compactOperations = await evaluate(
    devtools,
    sessionId,
    'globalThis.inspectManagementPresentation("operations")',
  )
  assert.equal(compactOperations.panelCount, 3)
  assert.equal(compactOperations.emptyCount, 1)
  assert.equal(compactOperations.noHorizontalOverflow, true)
  assert.equal(compactOperations.panelWithinPage, true)
})
