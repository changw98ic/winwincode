// SPDX-License-Identifier: Apache-2.0
//
// UI-608 key-page visual regression, in a real browser.
//
// Every key page is baselined at a desktop and a narrow viewport, plus the
// shell chrome both viewports share and the offline and route-failure states.
// Nothing here walks a flow: a functional E2E failure means a scenario broke,
// while a failure in this lane means a page started looking different.

import assert from 'node:assert/strict'
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
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
import {
  assertCredentialLeakFreeFile,
  scanCredentialLeakBytes,
} from '../scripts/credential-leak-gate.mjs'

const root = resolve(import.meta.dirname, '..')
const baselinePath = resolve(root, 'tests/fixtures/visual-regression/pages.baseline.json')
const artifactDirectory = resolve(root, '.cache/visual-regression')
const updateBaselines = process.env.WWC_VISUAL_BASELINE_WRITE === '1'

const ARTIFACT_REPORT = 'pages.report.txt'
const ARTIFACT_CAPTURE = 'pages.capture.json'

const DESKTOP = Object.freeze({ width: 1280, height: 900 })
const NARROW = Object.freeze({ width: 420, height: 900 })

const POPULATED_PAGES = Object.freeze([
  'home', 'chat', 'settings', 'attention', 'decisions', 'operations',
])
/**
 * One page load per fixture-data slice.  Inside a load the shell routes on the
 * hash, so every populated page is captured from a single mount.
 */
const LOAD_ROUTES = Object.freeze([
  { hash: '#/home', pages: POPULATED_PAGES, offline: true },
  { hash: '#/chat-empty', pages: [{ name: 'chat', id: 'chat-empty' }] },
])

test('every key page matches its committed visual baseline on desktop and at a narrow viewport', async t => {
  const chromePath = chromeBinary()
  assert.notEqual(chromePath, null, 'Chrome or Chromium is required for the UI-608 visual lane')
  command(root, 'corepack', ['pnpm', '--filter', '@winwincode/client', 'build'])
  const directory = mkdtempSync(join(tmpdir(), 'winwincode-ui608-pages-'))
  const certificateFiles = certificate(root, directory)
  const clientServer = staticClientServer({
    root,
    certificateFiles,
    fixturePath: 'tests/fixtures/browser-visual-pages.mjs',
    configuration: () => ({}),
  })
  const clientPort = await listen(clientServer)
  const clientOrigin = `https://client.localhost:${String(clientPort)}`
  let chrome = null
  let devtools = null
  let targetId = null
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

  async function open(hash) {
    if (targetId !== null) await devtools.send('Target.closeTarget', { targetId })
    const created = await devtools.send('Target.createTarget', { url: 'about:blank' })
    targetId = created.targetId
    const attached = await devtools.send('Target.attachToTarget', {
      targetId,
      flatten: true,
    })
    const sessionId = attached.sessionId
    await devtools.send('Page.enable', {}, sessionId)
    await devtools.send('Page.navigate', { url: `${clientOrigin}/${hash}` }, sessionId)
    await waitForGlobal(devtools, sessionId, 'captureVisualPage')
    return sessionId
  }

  async function setViewport(sessionId, viewport) {
    await devtools.send('Emulation.setDeviceMetricsOverride', {
      width: viewport.width,
      height: viewport.height,
      deviceScaleFactor: 1,
      mobile: false,
    }, sessionId)
  }

  const fingerprints = []
  const capturedJson = []

  for (const viewport of [DESKTOP, NARROW]) {
    for (const load of LOAD_ROUTES) {
      const sessionId = await open(load.hash)
      await setViewport(sessionId, viewport)

      const described = await evaluate(devtools, sessionId, 'globalThis.describeVisualViewport()')
      assert.equal(described.width, viewport.width, 'the viewport override must be in force')

      if (load.offline === true) fingerprints.push(
        ...await evaluate(devtools, sessionId, 'globalThis.captureVisualShell()'),
      )

      for (const entry of load.pages) {
        const name = typeof entry === 'string' ? entry : entry.name
        const captureId = typeof entry === 'string' ? undefined : entry.id
        fingerprints.push(await evaluate(devtools, sessionId, [
          'globalThis.captureVisualPage(',
          JSON.stringify(name),
          ',',
          JSON.stringify(captureId),
          ')',
        ].join('')))
      }

      if (load.offline === true) {
        fingerprints.push(...await evaluate(devtools, sessionId, 'globalThis.captureVisualOffline()'))
      }
      capturedJson.push(described)
    }
  }

  const ids = fingerprints.map(fingerprint => fingerprint.id)
  assert.equal(
    new Set(ids).size,
    ids.length,
    `each capture must have a unique id: ${JSON.stringify(ids)}`,
  )

  // Both viewports must have baselined the same key pages, which is what makes
  // a narrow-screen regression reviewable next to the desktop one.
  for (const name of POPULATED_PAGES) {
    for (const viewport of Object.entries({ desktop: DESKTOP, narrow: NARROW })) {
      const [label, size] = viewport
      assert.equal(
        ids.includes(`page/${name}@${label}`)
          && fingerprints.some(fingerprint => fingerprint.id === `page/${name}@${label}`
            && fingerprint.viewport.width === size.width),
        true,
        `page/${name} must be baselined at ${String(size.width)}px`,
      )
    }
  }
  assert.equal(ids.includes('shell/connection-offline@narrow'), true, 'the offline shell must be baselined')

  const capturedText = JSON.stringify(fingerprints)
  const captureScan = scanCredentialLeakBytes({
    bytes: Buffer.from(capturedText, 'utf8'),
    label: ARTIFACT_CAPTURE,
  })
  assert.equal(
    captureScan.status,
    'passed',
    `the page capture must stay free of credential material: `
      + `${JSON.stringify(captureScan.findings)}`,
  )

  if (updateBaselines) {
    mkdirSync(resolve(root, 'tests/fixtures/visual-regression'), { recursive: true })
    writeFileSync(baselinePath, `${JSON.stringify(fingerprints, null, 2)}\n`)
    process.stdout.write(`wrote ${baselinePath}\n`)
    return
  }

  const baseline = JSON.parse(readFileSync(baselinePath, 'utf8'))
  assert.equal(Array.isArray(baseline), true)
  const baselineById = new Map(baseline.map(fingerprint => [fingerprint.id, fingerprint]))
  assert.deepEqual(
    [...baselineById.keys()].sort(),
    [...ids].sort(),
    'the baseline and the capture must describe the same pages',
  )

  const differences = []
  const reports = []
  const sessionId = await open(LOAD_ROUTES[0].hash)
  for (const fingerprint of fingerprints) {
    const committed = baselineById.get(fingerprint.id)
    assert.notEqual(committed, undefined, `${fingerprint.id} is missing from the baseline`)
    const local = await evaluate(devtools, sessionId, [
      'globalThis.compareVisualPages(',
      JSON.stringify(committed),
      ',',
      JSON.stringify(fingerprint),
      ')',
    ].join(''))
    differences.push(...local.differences)
    if (local.differences.length > 0) reports.push(local.report)
  }

  mkdirSync(artifactDirectory, { recursive: true })
  writeFileSync(
    join(artifactDirectory, ARTIFACT_CAPTURE),
    `${JSON.stringify(fingerprints, null, 2)}\n`,
  )
  writeFileSync(
    join(artifactDirectory, ARTIFACT_REPORT),
    `${reports.join('\n') || 'no visual differences'}\n`,
  )
  assertCredentialLeakFreeFile(join(artifactDirectory, ARTIFACT_CAPTURE), {
    label: ARTIFACT_CAPTURE,
  })

  assert.equal(
    differences.length,
    0,
    `${String(differences.length)} page visual differences; review `
      + `${join('.cache/visual-regression', ARTIFACT_REPORT)}\n`
      + reports.join('\n'),
  )
})
