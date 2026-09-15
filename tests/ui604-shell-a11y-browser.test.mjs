// SPDX-License-Identifier: Apache-2.0

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

const SURFACES = ['chat', 'home', 'settings']

test('a real browser keeps one page heading, one live-region channel per page, and a keyboard bypass on every surface', async t => {
  const chromePath = chromeBinary()
  assert.notEqual(chromePath, null, 'Chrome or Chromium is required for the UI-604 audit')
  command(root, 'corepack', ['pnpm', '--filter', '@winwincode/client', 'build'])
  const directory = mkdtempSync(join(tmpdir(), 'winwincode-ui604-a11y-'))
  const certificateFiles = certificate(root, directory)
  const clientServer = staticClientServer({
    root,
    certificateFiles,
    fixturePath: 'tests/fixtures/browser-a11y-audit.mjs',
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

  async function open(hash) {
    await devtools.send('Page.navigate', { url: `${clientOrigin}/${hash}` }, sessionId)
    await waitForGlobal(devtools, sessionId, 'inspectAccessibility')
  }

  for (const surface of SURFACES) {
    await open(`#/${surface}`)
    const audit = await evaluate(
      devtools,
      sessionId,
      `globalThis.inspectAccessibility(${JSON.stringify(surface)})`,
    )

    assert.equal(audit.h1.length, 1, `${surface} must expose exactly one page heading`)
    assert.ok(audit.h1[0].length > 0, `${surface} must name its page heading`)
    assert.deepEqual(
      audit.skippedHeadingLevels,
      [],
      `${surface} must not skip a heading level: ${JSON.stringify(audit.headings)}`,
    )
    assert.equal(
      audit.surfaceSlotLive,
      null,
      `${surface} must not turn the whole surface slot into a live region`,
    )
    assert.deepEqual(
      audit.collectionLiveRegions,
      [],
      `${surface} must not re-announce a whole collection on every realtime render`,
    )
    assert.deepEqual(
      audit.unexpectedLiveRegions,
      audit.unexpectedLiveRegions.length === 0 ? [] : audit.unexpectedLiveRegions,
      `${surface} exposes live regions outside the audited allow-list`,
    )
    assert.deepEqual(
      audit.unexpectedLiveRegions,
      [],
      `${surface} exposes live regions outside the audited allow-list: `
        + `${JSON.stringify(audit.unexpectedLiveRegions)}`,
    )
    assert.equal(audit.landmarks.main, 1, `${surface} needs exactly one main landmark`)
    assert.equal(audit.landmarks.banner, 1, `${surface} needs the banner header`)
    assert.equal(audit.landmarks.navigation, 1, `${surface} needs the product-area navigation`)
    assert.equal(audit.landmarks.navigationLabel, '产品导航')
    assert.equal(audit.skipLink.present, true, `${surface} needs a keyboard bypass`)
    assert.equal(audit.skipLink.label, '跳到主内容')
    assert.equal(
      audit.skipLink.firstFocusable,
      true,
      `${surface} must put the bypass before the repeated navigation`,
    )
    assert.equal(audit.mainFocusable, true, `${surface} main must accept programmatic focus`)
    assert.equal(audit.noHorizontalOverflow, true, `${surface} must not scroll sideways`)
  }

  await open('#/settings')
  const skip = await evaluate(devtools, sessionId, 'globalThis.runSkipLinkScenario()')
  assert.notEqual(
    skip.hiddenClip,
    'none',
    'the bypass stays out of the visual order until focused',
  )
  assert.equal(
    skip.focusedClip,
    'none',
    'focusing the bypass reveals it',
  )
  assert.equal(skip.beforeHash, skip.afterHash, 'activating the bypass must not change the route')
  assert.equal(skip.focusAfterActivation, 'main', 'activating the bypass must move focus to main')
  assert.equal(skip.mainTag, 'MAIN')
  assert.equal(skip.mainTabIndex, -1)

  const settingsHeadings = await evaluate(
    devtools,
    sessionId,
    'globalThis.inspectHeadingLevels("settings")',
  )
  assert.deepEqual(
    settingsHeadings.map(heading => [heading.tag, heading.text]),
    [
      ['H2', '模型'],
      ['H2', '模型设置不可用'],
      ['H3', '备份与恢复'],
    ],
    'Settings must nest its page title above its panels without skipping a level',
  )

  await devtools.send('Emulation.setDeviceMetricsOverride', {
    width: 640,
    height: 1024,
    deviceScaleFactor: 1,
    mobile: false,
  }, sessionId)
  const zoomed = await evaluate(
    devtools,
    sessionId,
    'globalThis.inspectAccessibility("settings")',
  )
  assert.equal(
    zoomed.noHorizontalOverflow,
    true,
    '200% zoom (a 640px window on a 1280px design) must not lose content sideways',
  )
  assert.equal(zoomed.skipLink.present, true)
  assert.equal(zoomed.h1.length, 1)
  assert.deepEqual(zoomed.collectionLiveRegions, [])

  const providerForm = await evaluate(devtools, sessionId, `(() => {
    const panel = document.querySelector('.wwc-settings-route-form')
    const key = document.querySelector('#wwc-device-provider-key')
    return {
      present: panel !== null,
      keyType: key.type,
      keyDisabled: key.disabled,
      keyLabel: key.labels[0].textContent.trim(),
      unlabelled: [...panel.querySelectorAll('input, select')].filter(input => input.labels.length === 0).length,
    }
  })()`)
  assert.equal(providerForm.present, true, 'device Provider settings must render')
  assert.equal(providerForm.keyType, 'password')
  assert.equal(providerForm.keyDisabled, true, 'missing Device must disable credential input')
  assert.equal(providerForm.keyLabel, 'API Key')
  assert.equal(providerForm.unlabelled, 0, 'every Provider input needs an accessible label')

  await devtools.send('Emulation.setDeviceMetricsOverride', {
    width: 390, height: 844, deviceScaleFactor: 1, mobile: false,
  }, sessionId)
  await open('#/chat')
  await evaluate(devtools, sessionId, 'globalThis.inspectAccessibility("chat")')
  const composer = await evaluate(devtools, sessionId, `(() => {
    const input = document.querySelector('.wwc-chat-composer-input').getBoundingClientRect()
    const send = document.querySelector('.wwc-chat-send').getBoundingClientRect()
    return { top: input.top, bottom: send.bottom, viewport: innerHeight }
  })()`)
  assert.ok(composer.top >= 0 && composer.bottom <= composer.viewport,
    'the mobile composer and send button must fit in the viewport')

  const extensions = await evaluate(devtools, sessionId, `(async () => {
    const { mountExtensionsPage } = await import('/module/extensions-page.js')
    const root = document.createElement('div')
    document.body.append(root)
    const view = mountExtensionsPage({ root })
    const tabs = [...root.querySelectorAll('[role="tab"]')]
    const results = tabs.map(tab => {
      tab.click()
      return root.querySelector('[role="tabpanel"]:not([hidden])').textContent
    })
    view.close()
    root.remove()
    return results
  })()`)
  assert.equal(extensions.length, 3)
  assert.match(extensions[0], /插件暂不可用/u)
  assert.match(extensions[1], /添加技能/u)
  assert.match(extensions[2], /添加 MCP 服务/u)
  for (const text of extensions) assert.doesNotMatch(text, /已安装|已连接|Archify/u)

  const pairing = await evaluate(devtools, sessionId, `(async () => {
    const { mountOnboardingPage } = await import('/module/onboarding-page.js')
    const root = document.createElement('div')
    document.body.append(root)
    const calls = []
    const view = mountOnboardingPage({ root,
      connect: async (...args) => { calls.push(args) }, onSignOut() {},
    })
    const form = root.querySelector('form')
    form.requestSubmit()
    const invalidCalls = calls.length
    root.querySelector('#wwc-onboarding-device-id').value = '123 456 789'
    root.querySelector('#wwc-onboarding-code').value = '12345678'
    form.requestSubmit()
    await Promise.resolve()
    view.close()
    root.remove()
    return { invalidCalls, calls }
  })()`)
  assert.deepEqual(pairing, { invalidCalls: 0, calls: [['123456789', '12345678']] })
})
