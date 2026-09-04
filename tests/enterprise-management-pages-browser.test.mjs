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

test('real browser keeps enterprise management readable, signalled, and focused at desktop and compact widths', async t => {
  const chromePath = chromeBinary()
  assert.notEqual(chromePath, null, 'Chrome or Chromium is required for the enterprise UI browser test')
  command(root, 'corepack', ['pnpm', '--filter', '@winwincode/client', 'build'])
  const directory = mkdtempSync(join(tmpdir(), 'winwincode-enterprise-ui-'))
  const certificateFiles = certificate(root, directory)
  const clientServer = staticClientServer({
    root,
    certificateFiles,
    fixturePath: 'tests/fixtures/browser-enterprise-management-pages.mjs',
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
    width: 1280,
    height: 900,
    deviceScaleFactor: 1,
    mobile: false,
  }, sessionId)
  await devtools.send('Page.navigate', {
    url: `https://client.localhost:${String(clientPort)}/`,
  }, sessionId)
  await waitForGlobal(devtools, sessionId, 'inspectEnterpriseManagement')

  const desktop = await evaluate(
    devtools,
    sessionId,
    'globalThis.inspectEnterpriseManagement()',
  )
  assert.equal(desktop.resourcesPage, 'management')
  assert.equal(desktop.operationsPage, 'management')
  assert.equal(desktop.resourcePanels, 4)
  assert.equal(desktop.operationsPanels, 5)
  assert.equal(desktop.resourceEmpty, 5)
  assert.equal(desktop.operationsEmpty, 5)
  assert.equal(desktop.deniedFieldsetDisabled, true)
  assert.equal(desktop.availableFieldsetDisabled, false)
  assert.notEqual(desktop.statusIcon, '')
  assert.equal(desktop.statusIconHidden, 'true')
  assert.equal(desktop.statusRole, 'status')
  assert.equal(desktop.noHorizontalOverflow, true)
  assert.equal(desktop.panelsShareRow, true)

  const focus = await evaluate(devtools, sessionId, 'globalThis.inspectEnterpriseFocus()')
  assert.equal(focus.active, true)
  assert.equal(focus.outlineStyle, 'solid')
  assert.equal(focus.outlineWidth, '3px')

  await evaluate(devtools, sessionId, 'globalThis.setEnterpriseBusy(true)')
  const busy = await evaluate(devtools, sessionId, 'globalThis.inspectEnterpriseManagement()')
  assert.equal(busy.busy, 'true')

  await devtools.send('Emulation.setDeviceMetricsOverride', {
    width: 360,
    height: 800,
    deviceScaleFactor: 1,
    mobile: false,
  }, sessionId)
  const compact = await evaluate(
    devtools,
    sessionId,
    'globalThis.inspectEnterpriseManagement()',
  )
  assert.equal(compact.noHorizontalOverflow, true)
  assert.equal(compact.panelsStacked, true)
})
