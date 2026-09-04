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
import {
  LARGE_DATA_CORPUS,
  LARGE_DATA_PERFORMANCE_BASELINE,
} from './fixtures/ui605-large-data.mjs'

const root = resolve(import.meta.dirname, '..')
const rowBudget = LARGE_DATA_PERFORMANCE_BASELINE.rendered.listRows

test('a real browser keeps the 5 000 Delivery page inside the recorded budgets', async t => {
  const chromePath = chromeBinary()
  assert.notEqual(
    chromePath,
    null,
    'Chrome or Chromium is required for the UI-605 performance baseline',
  )
  command(root, 'corepack', ['pnpm', '--filter', '@winwincode/client', 'build'])
  const directory = mkdtempSync(join(tmpdir(), 'winwincode-ui605-large-list-'))
  const certificateFiles = certificate(root, directory)
  const clientServer = staticClientServer({
    root,
    certificateFiles,
    fixturePath: 'tests/fixtures/browser-ui605-large-list.mjs',
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
    height: 2_400,
    deviceScaleFactor: 1,
    mobile: false,
  }, sessionId)
  await devtools.send('Page.navigate', {
    url: `https://client.localhost:${String(clientPort)}/#/strongflow`,
  }, sessionId)
  await waitForGlobal(devtools, sessionId, 'runUi605LargeListScenario')
  const result = await evaluate(devtools, sessionId, 'globalThis.runUi605LargeListScenario()')

  // The corpus is enterprise sized and the DOM is not.
  assert.equal(result.corpus, LARGE_DATA_CORPUS.deliveries)
  assert.equal(
    result.rowsAtMount,
    rowBudget,
    `the window rendered ${String(result.rowsAtMount)} rows`,
  )
  assert.ok(
    result.rowsAtMount < LARGE_DATA_CORPUS.deliveries,
    'the whole corpus must not be mounted',
  )
  assert.ok(
    result.nodesAtMount <= LARGE_DATA_PERFORMANCE_BASELINE.rendered.pageDomNodes,
    `${String(result.nodesAtMount)} DOM nodes for a ${
      String(LARGE_DATA_CORPUS.deliveries)
    } Delivery corpus`,
  )
  assert.equal(result.nodesAfterSearch, result.nodesAtMount, 'searching changed the DOM size')
  assert.equal(result.nodesFiltered, result.nodesAtMount, 'filtering changed the DOM size')
  assert.equal(
    result.filteredRows,
    rowBudget,
    `the filtered window rendered ${String(result.filteredRows)} rows`,
  )
  assert.equal(
    result.nodesAfterScroll,
    result.nodesAtMount,
    'scrolling through the corpus changed the DOM size',
  )
  assert.equal(result.rowsAfterScroll, rowBudget, 'scrolling left a partial window')

  // Recorded interaction budgets.  The millisecond budgets are intentionally
  // wide; the DOM budgets above are the deterministic guard.
  assert.ok(
    result.firstInteractionMillis <= LARGE_DATA_PERFORMANCE_BASELINE.millis.firstInteraction,
    `first interaction took ${result.firstInteractionMillis.toFixed(1)}ms`,
  )
  assert.ok(
    result.scrollMillis <= LARGE_DATA_PERFORMANCE_BASELINE.millis.scroll,
    `${String(result.scrollSteps)} scroll steps took ${result.scrollMillis.toFixed(1)}ms`,
  )
  assert.ok(result.searchAccepted, 'the search control never reached the model')
  assert.equal(result.deepLinkRendered, true, 'the deep-linked Delivery was not revealed')
  assert.match(result.note, new RegExp(`Rendered ${String(rowBudget)} of `, 'u'))
  assert.match(result.note, /not rendered in this window/u)

  process.stdout.write(
    `ui605 large-data baseline: mount ${result.mountedMillis.toFixed(1)}ms, `
      + `first interaction ${result.firstInteractionMillis.toFixed(1)}ms, `
      + `${String(result.scrollSteps)} scroll steps ${result.scrollMillis.toFixed(1)}ms, `
      + `${String(result.nodesAtMount)} DOM nodes for ${
        String(result.corpus)
      } Deliveries\n`,
  )
})
