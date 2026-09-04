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

test('real Chrome completes the first-run checklist from a fresh install and reopens it from diagnostics', async t => {
  const chromePath = chromeBinary()
  assert.notEqual(chromePath, null, 'Chrome or Chromium is required for readiness validation')
  command(root, 'corepack', ['pnpm', '--filter', '@winwincode/client', 'build'])
  const directory = mkdtempSync(join(tmpdir(), 'winwincode-readiness-'))
  const certificateFiles = certificate(root, directory)
  const clientServer = staticClientServer({
    root,
    certificateFiles,
    fixturePath: 'tests/fixtures/browser-readiness.mjs',
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
    url: `https://client.localhost:${String(clientPort)}/#/settings`,
  }, sessionId)
  await waitForGlobal(devtools, sessionId, 'readinessReady')

  const fresh = await evaluate(devtools, sessionId, 'globalThis.inspectChecklist()')
  assert.equal(fresh.present, true)
  assert.equal(fresh.hidden, false)
  assert.equal(fresh.expanded, true)
  assert.match(fresh.summary, /1 of 6 complete/u)
  assert.equal(fresh.items.length, 6)
  const byId = Object.fromEntries(fresh.items.map(item => [item.id, item]))
  assert.equal(byId['repository-scope'].status, 'ready')
  assert.match(byId['repository-scope'].reason, /Complete/u)
  assert.equal(byId['model-route'].status, 'attention')
  assert.match(byId['model-route'].reason, /provider is configured/iu)
  assert.match(byId['model-route'].checkedAt, /Checked \d{4}-\d{2}-\d{2}T/u)
  assert.match(byId['model-route'].fixHref, /^#\/settings\?organizationId=/u)
  assert.match(byId['model-route'].fixLabel, /Open Settings/u)
  assert.equal(byId['server-worker-health'].status, 'attention')
  assert.match(byId['server-worker-health'].reason, /No local Worker is registered/iu)
  assert.equal(byId['first-chat-delivery'].status, 'attention')
  assert.match(byId['first-chat-delivery'].fixLabel, /Start your first Chat/u)
  assert.equal(fresh.leak, false, 'no secret material may reach the checklist')

  const navigated = await evaluate(devtools, sessionId, 'globalThis.clickModelRouteFix()')
  assert.match(navigated.hash, /organizationId=/u)
  assert.match(navigated.hash, /repositoryId=rep_00000000000000000000000001/u)

  const completed = await evaluate(devtools, sessionId, 'globalThis.completeAllSteps()')
  assert.match(completed.summary, /6 of 6 complete/u)
  assert.equal(completed.items.every(item => item.status === 'ready'), true)
  assert.equal(completed.items.every(item => item.checkedAt !== null), true)
  assert.equal(
    completed.items.some(item => item.fixLabel !== null),
    false,
    'ready items expose no fix entry',
  )
  assert.equal(completed.leak, false)

  const collapsed = await evaluate(devtools, sessionId, 'globalThis.collapseChecklist()')
  assert.equal(collapsed.expanded, false)
  assert.equal(collapsed.itemsHidden, true)
  assert.match(collapsed.summary, /6 of 6 complete/u)

  const reopened = await evaluate(devtools, sessionId, 'globalThis.openFromDiagnostics()')
  assert.equal(reopened.reopenPresent, true)
  assert.equal(reopened.expanded, true)
  assert.equal(reopened.itemsHidden, false)
  assert.match(reopened.summary, /6 of 6 complete/u)
  assert.equal(reopened.leak, false)
})
