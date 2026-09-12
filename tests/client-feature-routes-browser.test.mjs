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
test('real browser routes mount Settings and the filtered task board without empty slots', async t => {
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

  async function open(path, featureRoute) {
    await devtools.send('Page.navigate', { url: `${clientOrigin}/${path}` }, sessionId)
    await waitForGlobal(devtools, sessionId, 'inspectFeatureRoute')
    await evaluate(
      devtools,
      sessionId,
      `globalThis.inspectFeatureRoute(${JSON.stringify(featureRoute)})`,
    )
  }

  await open('#/settings', 'settings')
  const navigation = await evaluate(
    devtools,
    sessionId,
    'globalThis.runFeatureNavigationScenario()',
  )
  assert.equal(navigation.settings.hash, '#/settings')
  assert.match(navigation.settings.status, /^就绪/u)
  assert.match(navigation.taskBoard.hash, /^#\/home\?filter=attention/u)
  assert.notEqual(navigation.taskBoard.status, '')
  assert.match(navigation.denied, /没有访问/u)
  assert.doesNotMatch(navigation.denied, /private route fixture/iu)
  assert.match(navigation.network, /无法连接/u)
  assert.doesNotMatch(navigation.network, /private route fixture/iu)
  assert.deepEqual(navigation.calls.abortedQueries, ['settings.get'])
  assert.match(navigation.afterCancellation.status, /^就绪/u)

  await open('#/settings', 'settings')
  const desktopSettings = await evaluate(
    devtools,
    sessionId,
    'globalThis.inspectManagementPresentation("settings")',
  )
  assert.equal(desktopSettings.page, 'management')
  assert.equal(desktopSettings.panelCount, 5)
  assert.equal(desktopSettings.emptyCount, 1)
  assert.notEqual(desktopSettings.statusIcon, '')
  assert.equal(desktopSettings.statusIconHidden, 'true')
  assert.equal(desktopSettings.statusRole, 'status')
  assert.equal(desktopSettings.noHorizontalOverflow, true)
  const focus = await evaluate(
    devtools,
    sessionId,
    'globalThis.inspectManagementFocus("#wwc-settings-category")',
  )
  assert.equal(focus.active, true)
  assert.equal(focus.outlineStyle, 'solid')
  assert.equal(focus.outlineWidth, '2px')

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

  await open('#/home?filter=attention', 'task-board')
  const taskBoard = await evaluate(
    devtools,
    sessionId,
    'globalThis.inspectFeatureRoute("task-board")',
  )
  assert.match(taskBoard.hash, /^#\/home\?filter=attention/u)
  assert.notEqual(taskBoard.status, '')
})
