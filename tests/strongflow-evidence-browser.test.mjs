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

test('a real browser opens Evidence, Tests, and Logs tabs with exact bindings and deep links', async t => {
  const chromePath = chromeBinary()
  assert.notEqual(
    chromePath,
    null,
    'Chrome or Chromium is required for the Evidence workbench test',
  )
  command(root, 'corepack', ['pnpm', '--filter', '@winwincode/client', 'build'])
  const directory = mkdtempSync(join(tmpdir(), 'winwincode-evidence-strongflow-'))
  const certificateFiles = certificate(root, directory)
  const clientServer = staticClientServer({
    root,
    certificateFiles,
    fixturePath: 'tests/fixtures/browser-strongflow-evidence-client.mjs',
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
    url: `https://client.localhost:${String(clientPort)}/#/strongflow`,
  }, sessionId)
  await waitForGlobal(devtools, sessionId, 'runEvidenceWorkbenchScenario')
  const result = await evaluate(devtools, sessionId, 'globalThis.runEvidenceWorkbenchScenario()')

  assert.match(result.initial.hash, /^#\/strongflow\?delivery=dlv_00000000000000000000000002/u)
  assert.deepEqual(result.initial.tabLabels, ['Evidence', 'Tests', 'Logs'])
  assert.equal(result.initial.selected, 'Evidence')
  assert.deepEqual(result.initial.rowTypes, ['test', 'command', 'runtime_event', 'diff'])
  assert.deepEqual(result.initial.candidateStates, ['current'])

  assert.deepEqual(result.testsView.rowTypes, ['test'])
  assert.match(result.testsView.hash, /tab=tests/u)
  assert.match(result.testsView.hash, /delivery=dlv_00000000000000000000000002/u)

  assert.equal(result.detail.outcome, 'Failed · business')
  assert.equal(result.detail.tone, 'business-fail')
  assert.equal(result.detail.candidate, 'current candidate')
  assert.match(result.detail.artifact, /not available/u)
  assert.match(result.detail.hash, /evidence=evidence%3A1/u)
  assert.match(result.detail.hash, /tab=tests/u)

  assert.equal(result.closed.hash.includes('evidence='), false)
  assert.equal(result.navigationEntryCount, 1)

  assert.deepEqual(result.evidenceQueries, [{
    evidenceId: 'evidence:1',
    readPageLimit: 1,
    cursorToken: 'cursor_00000000000000000000000000000002',
    page: { cursor: null, limit: 1 },
  }])
  assert.equal(result.contentQueries, 0)
})
