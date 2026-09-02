import assert from 'node:assert/strict'
import { existsSync, mkdtempSync, rmSync } from 'node:fs'
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
const fixturePath = 'tests/fixtures/browser-strongflow-workbench-layout.mjs'

test('real Chrome keeps the complete StrongFlow workbench reachable across layouts', async t => {
  const chromePath = chromeBinary()
  if (chromePath === null) {
    t.skip('Chrome is not installed')
    return
  }
  command(root, 'corepack', ['pnpm', '--filter', '@winwincode/client', 'build'])
  const directory = mkdtempSync(join(tmpdir(), 'winwincode-strongflow-layout-'))
  const certificateFiles = certificate(root, directory)
  const clientServer = staticClientServer({
    root,
    certificateFiles,
    fixturePath,
    configuration: () => ({}),
  })
  const clientPort = await listen(clientServer)
  const clientOrigin = `https://client.localhost:${String(clientPort)}`
  const launched = await DevTools.launch({
    chromePath,
    directory,
    debugPort: await freePort(),
  })
  const { chrome, devtools } = launched
  const exceptions = []
  let sessionId = null
  t.after(async () => {
    devtools.close()
    await Promise.all([
      stopChild(chrome, 'SIGTERM'),
      closeServer(clientServer),
    ])
    rmSync(directory, { recursive: true, force: true })
    assert.equal(existsSync(directory), false)
  })
  devtools.on('Runtime.exceptionThrown', event => { exceptions.push(event) })
  const { targetId } = await devtools.send('Target.createTarget', { url: 'about:blank' })
  ;({ sessionId } = await devtools.send('Target.attachToTarget', { targetId, flatten: true }))
  await devtools.send('Runtime.enable', {}, sessionId)
  await devtools.send('Page.enable', {}, sessionId)
  await devtools.send('Emulation.setDeviceMetricsOverride', {
    width: 1440,
    height: 1000,
    deviceScaleFactor: 1,
    mobile: false,
  }, sessionId)
  await devtools.send('Page.navigate', { url: clientOrigin }, sessionId)
  await waitForGlobal(devtools, sessionId, 'runStrongFlowWideLayoutScenario')

  const wide = await evaluate(devtools, sessionId, 'runStrongFlowWideLayoutScenario()')
  assert.equal(wide.before.layout.viewport, 'wide')
  assert.deepEqual(wide.before.regions.map(region => region.tag), ['NAV', 'SECTION', 'ASIDE'])
  assert.deepEqual(wide.before.regions.map(region => region.visible), [true, true, true])
  assert.deepEqual(wide.before.regions.map(region => region.ariaLabel), [
    'Delivery and Task navigation',
    'Delivery main content',
    'Attention and Evidence context',
  ])
  assert.ok(
    wide.before.regions[0].x < wide.before.regions[1].x,
    JSON.stringify(wide.before),
  )
  assert.ok(
    wide.before.regions[1].x < wide.before.regions[2].x,
    JSON.stringify(wide.before),
  )
  assert.deepEqual(wide.navigationHandle, {
    ariaControls: 'wwc-strongflow-navigation',
    ariaOrientation: 'vertical',
    ariaValueNow: '24',
    role: 'separator',
  })
  assert.equal(wide.after.navigationWidth, '24')
  assert.equal(wide.after.navigationCollapsed, 'true')
  assert.equal(wide.after.artifact, 'candidate')
  assert.equal(wide.after.candidateVisible, true)

  await devtools.send('Page.reload', { ignoreCache: true }, sessionId)
  await waitForGlobal(devtools, sessionId, 'runStrongFlowRestoredLayoutScenario')
  const restored = await evaluate(devtools, sessionId, 'runStrongFlowRestoredLayoutScenario()')
  assert.equal(restored.navigationWidth, '24')
  assert.equal(restored.navigationCollapsed, 'true')
  assert.equal(restored.artifact, 'candidate')
  assert.equal(restored.candidateVisible, true)

  await devtools.send('Emulation.setDeviceMetricsOverride', {
    width: 420,
    height: 900,
    deviceScaleFactor: 1,
    mobile: true,
  }, sessionId)
  const narrow = await evaluate(devtools, sessionId, 'runStrongFlowNarrowLayoutScenario()')
  assert.equal(narrow.layout.viewport, 'narrow')
  assert.deepEqual(narrow.resizeHandlesHidden, [true, true])
  assert.deepEqual(narrow.navigationOpen, {
    activeClass: 'wwc-drawer-close',
    drawerVisible: true,
    role: 'dialog',
    taskVisible: true,
  })
  assert.deepEqual(narrow.afterEscape, { drawerHidden: true, focusReturned: true })
  assert.deepEqual(narrow.contextOpen, {
    attentionVisible: true,
    drawerVisible: true,
    evidenceVisible: true,
  })
  assert.equal(narrow.approvalVisible, true)
  assert.equal(narrow.candidateVisible, true)
  assert.equal(exceptions.length, 0, JSON.stringify(exceptions))
})
