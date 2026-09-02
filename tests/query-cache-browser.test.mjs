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

test('real Chrome coalesces repeated invalidations and retains the Settings draft', async t => {
  const chromePath = chromeBinary()
  if (chromePath === null) {
    t.skip('Chrome is not installed')
    return
  }
  command(root, 'corepack', ['pnpm', '--filter', '@winwincode/client', 'build'])
  const directory = mkdtempSync(join(tmpdir(), 'winwincode-query-cache-'))
  const certificateFiles = certificate(root, directory)
  const clientServer = staticClientServer({
    root,
    certificateFiles,
    fixturePath: 'tests/fixtures/browser-query-cache-client.mjs',
    configuration: () => ({}),
  })
  const clientPort = await listen(clientServer)
  const launched = await DevTools.launch({
    chromePath,
    directory,
    debugPort: await freePort(),
  })
  const { chrome, devtools } = launched
  const exceptions = []
  t.after(async () => {
    devtools.close()
    await Promise.all([stopChild(chrome, 'SIGTERM'), closeServer(clientServer)])
    rmSync(directory, { recursive: true, force: true })
    assert.equal(existsSync(directory), false)
  })
  devtools.on('Runtime.exceptionThrown', event => { exceptions.push(event) })
  const { targetId } = await devtools.send('Target.createTarget', { url: 'about:blank' })
  const { sessionId } = await devtools.send('Target.attachToTarget', { targetId, flatten: true })
  await devtools.send('Runtime.enable', {}, sessionId)
  await devtools.send('Page.enable', {}, sessionId)
  await devtools.send('Page.navigate', {
    url: `https://client.localhost:${String(clientPort)}`,
  }, sessionId)
  await waitForGlobal(devtools, sessionId, 'runQueryCacheScenario')

  const result = await evaluate(devtools, sessionId, 'runQueryCacheScenario()')
  assert.deepEqual(result.duringReload, {
    ariaBusy: 'true',
    draft: 'draft-provider',
    focused: true,
    revision: 1,
  })
  assert.deepEqual(result.afterReload, {
    draft: 'draft-provider',
    focused: true,
    realtime: 'subscribed',
    revision: 2,
    selectionStart: 5,
    status: 'ready',
  })
  assert.ok(result.queryDelta.settings <= 2, JSON.stringify(result.queryDelta))
  assert.ok(result.queryDelta.credentials <= 1, JSON.stringify(result.queryDelta))
  assert.equal(exceptions.length, 0, JSON.stringify(exceptions))
})
