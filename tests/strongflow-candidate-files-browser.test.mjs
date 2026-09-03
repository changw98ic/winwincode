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

test('real Chrome reviews a bounded Candidate tree and drives the linked Diff by keyboard', async t => {
  const chromePath = chromeBinary()
  assert.notEqual(chromePath, null, 'Chrome or Chromium is required for the Candidate files test')
  command(root, 'corepack', ['pnpm', '--filter', '@winwincode/client', 'build'])
  const directory = mkdtempSync(join(tmpdir(), 'winwincode-candidate-files-'))
  const certificateFiles = certificate(root, directory)
  const clientServer = staticClientServer({
    root,
    certificateFiles,
    fixturePath: 'tests/fixtures/browser-strongflow-candidate-files.mjs',
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
  const { sessionId } = await devtools.send('Target.attachToTarget', { targetId, flatten: true })
  await devtools.send('Runtime.enable', {}, sessionId)
  await devtools.send('Page.enable', {}, sessionId)
  await devtools.send('Page.navigate', {
    url: `https://client.localhost:${String(clientPort)}/#/strongflow?delivery=dlv_00000000000000000000000001&session=psn_00000000000000000000000001&stageRun=run_00000000000000000000000001&file=src%2Fcurrent.ts`,
  }, sessionId)
  await waitForGlobal(devtools, sessionId, 'runCandidateFilesScenario')
  const result = await evaluate(devtools, sessionId, 'globalThis.runCandidateFilesScenario()')

  assert.equal(result.initial.rowCount <= 200, true)
  assert.match(result.initial.summary, /230 files/u)
  assert.match(result.initial.renamed, /src\/legacy\.ts/u)
  assert.match(result.initial.unavailable, /Binary/u)
  assert.deepEqual(new Set(result.initial.statusLabels), new Set([
    'Added', 'Modified', 'Deleted', 'Renamed', 'Copied', 'Type changed',
  ]))
  assert.equal(result.initial.selectedPath, 'src/current.ts')
  assert.equal(result.initial.candidateSummaryHasTechnicalIdentity, false)
  assert.equal(result.initial.technicalOpen, false)
  assert.match(result.initial.technicalText, new RegExp(`sha256:${'3'.repeat(64)}`, 'u'))

  assert.deepEqual(result.collapsed, { expanded: 'false', containsCurrent: false })
  assert.notEqual(result.keyboard.target, 'src/current.ts')
  assert.equal(result.keyboard.selectedPath, result.keyboard.target)
  assert.equal(result.keyboard.activePath, result.keyboard.target)
  assert.match(
    result.keyboard.hash,
    new RegExp(`&file=${encodeURIComponent(result.keyboard.target)}&view=unified$`, 'u'),
  )
  assert.match(result.keyboard.diff, new RegExp(result.keyboard.target.replace(/[.*+?^${}()|[\]\\]/gu, '\\$&'), 'u'))

  assert.deepEqual(result.filtered.paths, ['src/current.ts'])
  assert.equal(result.filtered.count, 2)
  assert.match(result.binary.hash, /&file=public%2Flogo\.png&view=unified$/u)
  assert.match(result.binary.status, /Binary file preview is unavailable/u)
  assert.equal(result.calls.some(call => call[0] === 'loadMoreCandidateFiles'), true)
  assert.equal(result.mainCount, 0)
})
