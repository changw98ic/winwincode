// SPDX-License-Identifier: Apache-2.0
//
// UI-608 component-state visual regression, in a real browser.
//
// This lane is deliberately not a functional scenario: nothing is clicked
// through, no command is asserted, and a failure here names the component state
// and the visual property that moved.  A functional E2E failure means a flow
// broke; a failure here means the product started looking different.

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
const baselinePath = resolve(root, 'tests/fixtures/visual-regression/component-states.baseline.json')
const artifactDirectory = resolve(root, '.cache/visual-regression')
const updateBaselines = process.env.WWC_VISUAL_BASELINE_WRITE === '1'

const ARTIFACT_REPORT = 'component-states.report.txt'
const ARTIFACT_CAPTURE = 'component-states.capture.json'

test('every component state renders exactly the committed visual baseline', async t => {
  const chromePath = chromeBinary()
  assert.notEqual(chromePath, null, 'Chrome or Chromium is required for the UI-608 visual lane')
  command(root, 'corepack', ['pnpm', '--filter', '@winwincode/client', 'build'])
  const directory = mkdtempSync(join(tmpdir(), 'winwincode-ui608-components-'))
  const certificateFiles = certificate(root, directory)
  const clientServer = staticClientServer({
    root,
    certificateFiles,
    fixturePath: 'tests/fixtures/browser-visual-component-states.mjs',
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
    url: `https://client.localhost:${String(clientPort)}/#/home`,
  }, sessionId)
  await waitForGlobal(devtools, sessionId, 'captureComponentStates')

  // The fixture can only produce comparable output when the product stylesheet
  // and its design tokens actually loaded with the page.
  const ready = await evaluate(devtools, sessionId, 'globalThis.inspectGalleryReadiness()')
  assert.equal(ready.fontOverride.includes('ui-monospace'), true, 'the fixed font contract must be installed')
  assert.equal(ready.entries > 0, true, 'the component-state gallery must not be empty')

  const capture = await evaluate(devtools, sessionId, 'globalThis.captureComponentStates()')
  assert.equal(capture.schemaVersion, 1)
  assert.equal(capture.fingerprints.length, capture.entryIds.length)
  assert.equal(new Set(capture.entryIds).size, capture.entryIds.length, 'entry ids must be unique')

  // The baseline names design tokens, so the capture must actually resolve the
  // product palette rather than falling back to raw colours.
  const tokenisedValues = capture.fingerprints.flatMap(fingerprint => fingerprint.nodes)
    .flatMap(node => Object.values(node.style))
    .filter(value => value.startsWith('var(--wwc-'))
  assert.equal(
    tokenisedValues.length > 0,
    true,
    'the capture must resolve design tokens into the recorded presentation',
  )

  // Every visual state the decision names must be present in this lane.
  const ids = new Set(capture.entryIds)
  for (const required of [
    'button/default',
    'button/busy',
    'button/disabled',
    'button/destructive',
    'status-badge/warning',
    'status-badge/danger',
    'panel/busy',
    'form-field/error',
    'empty-state/default',
    'error-state/default',
    'client-error-boundary/error',
    'connection-bar/reconnecting',
    'connection-bar/offline',
    'strongflow-diff/unified',
    'strongflow-diff/error',
  ]) assert.equal(ids.has(required), true, `${required} must be part of the component-state lane`)

  // The capture is reviewed in CI, so it must carry no credential material.
  const captureScan = scanCredentialLeakBytes({
    bytes: Buffer.from(capture.capturedText, 'utf8'),
    label: ARTIFACT_CAPTURE,
  })
  assert.equal(
    captureScan.status,
    'passed',
    `the component-state capture must stay free of credential material: `
      + `${JSON.stringify(captureScan.findings)}`,
  )

  // Each state the decision names maps to the entry that baselines it, so a
  // renamed entry cannot silently drop a state from the lane.
  const REQUIRED_STATE_COVERAGE = Object.freeze({
    default: 'button/default',
    busy: 'button/busy',
    disabled: 'button/disabled',
    error: 'error-state/default',
    empty: 'empty-state/default',
    stale: 'connection-bar/reconnecting',
    destructive: 'button/destructive',
  })
  for (const [state, entry] of Object.entries(REQUIRED_STATE_COVERAGE)) {
    assert.equal(
      ids.has(entry),
      true,
      `the component-state lane must cover the ${state} state through ${entry}`,
    )
  }
  if (updateBaselines) {
    mkdirSync(resolve(root, 'tests/fixtures/visual-regression'), { recursive: true })
    writeFileSync(baselinePath, `${JSON.stringify(capture.fingerprints, null, 2)}\n`)
    process.stdout.write(`wrote ${baselinePath}\n`)
    return
  }

  const baseline = JSON.parse(readFileSync(baselinePath, 'utf8'))
  assert.equal(Array.isArray(baseline), true)
  const baselineById = new Map(baseline.map(fingerprint => [fingerprint.id, fingerprint]))
  assert.deepEqual(
    [...baselineById.keys()].sort(),
    [...capture.entryIds].sort(),
    'the baseline and the capture must describe the same component states',
  )

  const comparison = await evaluate(devtools, sessionId, [
    'globalThis.compareComponentStates(',
    `${JSON.stringify(baseline)},`,
    `${JSON.stringify(capture.fingerprints)})`,
  ].join(''))
  const differences = comparison.differences

  mkdirSync(artifactDirectory, { recursive: true })
  writeFileSync(
    join(artifactDirectory, ARTIFACT_CAPTURE),
    `${JSON.stringify(capture.fingerprints, null, 2)}\n`,
  )
  writeFileSync(
    join(artifactDirectory, ARTIFACT_REPORT),
    `${comparison.report}\n`,
  )
  assertCredentialLeakFreeFile(join(artifactDirectory, ARTIFACT_CAPTURE), {
    label: ARTIFACT_CAPTURE,
  })

  assert.equal(
    differences.length,
    0,
    `${differences.length} component-state visual differences; review `
      + `${join('.cache/visual-regression', ARTIFACT_REPORT)}\n`
      + comparison.report,
  )

  // A gate that cannot fail is not a gate.  Mutating the baseline the way a
  // real palette change would must be reported, with the reason a reviewer
  // needs to tell a palette change apart from a layout change.
  const mutatedBaseline = baseline.map(fingerprint => (
    fingerprint.id === 'button/destructive'
      ? {
          ...fingerprint,
          nodes: fingerprint.nodes.map(node => ({
            ...node,
            style: { ...node.style, 'background-color': 'rgb(1, 2, 3)' },
          })),
        }
      : fingerprint
  ))
  const selfCheck = await evaluate(devtools, sessionId, [
    'globalThis.compareComponentStates(',
    JSON.stringify(mutatedBaseline),
    ',',
    JSON.stringify(capture.fingerprints),
    ')',
  ].join(''))
  assert.deepEqual(
    selfCheck.differences.map(difference => [difference.reason, difference.property]),
    [['style', 'style.background-color']],
  )
})
