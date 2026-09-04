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

const SURFACES = ['chat', 'settings', 'attention', 'operations', 'decisions']

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
    await open(`#/${surface === 'chat' ? 'chat' : surface === 'decisions'
      ? `attention?session=psn_00000000000000000000000001`
      : surface === 'operations' ? 'settings/runtime' : surface}`)
    const audit = await evaluate(
      devtools,
      sessionId,
      `globalThis.inspectAccessibility(${JSON.stringify(surface)})`,
    )

    assert.equal(audit.h1.length, 1, `${surface} must expose exactly one page heading`)
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
    assert.equal(audit.landmarks.navigationLabel, 'Product areas')
    assert.equal(audit.skipLink.present, true, `${surface} needs a keyboard bypass`)
    assert.equal(audit.skipLink.label, 'Skip to main content')
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
      ['H2', 'Local Provider settings'],
      ['H2', 'Provider settings unavailable'],
      ['H3', 'Default model route'],
      ['H3', 'Add Credential reference'],
      ['H3', 'Credential references'],
      ['H3', 'No Credential references'],
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
})
