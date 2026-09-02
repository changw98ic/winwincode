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
const fixturePath = 'tests/fixtures/browser-strongflow-event-reload-client.mjs'

test('real Chrome retains StrongFlow review work across event reloads', async t => {
  const chromePath = chromeBinary()
  if (chromePath === null) {
    t.skip('Chrome is not installed')
    return
  }
  command(root, 'corepack', ['pnpm', '--filter', '@winwincode/client', 'build'])
  const directory = mkdtempSync(join(tmpdir(), 'winwincode-strongflow-event-'))
  const certificateFiles = certificate(root, directory)
  const clientServer = staticClientServer({
    root,
    certificateFiles,
    fixturePath,
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
  let sessionId = null
  t.after(async () => {
    devtools.close()
    await Promise.all([stopChild(chrome, 'SIGTERM'), closeServer(clientServer)])
    rmSync(directory, { recursive: true, force: true })
    assert.equal(existsSync(directory), false)
  })
  devtools.on('Runtime.exceptionThrown', event => { exceptions.push(event) })
  const { targetId } = await devtools.send('Target.createTarget', { url: 'about:blank' })
  ;({ sessionId } = await devtools.send('Target.attachToTarget', { targetId, flatten: true }))
  await devtools.send('Runtime.enable', {}, sessionId)
  await devtools.send('Page.enable', {}, sessionId)
  await devtools.send('Page.navigate', {
    url: `https://client.localhost:${String(clientPort)}`,
  }, sessionId)
  await waitForGlobal(devtools, sessionId, 'runStrongFlowEventReloadScenario')

  const result = await evaluate(devtools, sessionId, 'runStrongFlowEventReloadScenario()')
  assert.deepEqual(result.duringReload, {
    candidateRetained: true,
    changesDraft: 'Requested changes retained in Chrome',
    currentDeliveryRetained: true,
    diagramsRetained: true,
    draft: 'Review draft retained in Chrome',
    reviewDisabled: true,
  })
  assert.deepEqual(result.afterEquivalentEvents, {
    candidateRetained: true,
    changesDraft: 'Requested changes retained in Chrome',
    diagramsRetained: true,
    draft: 'Review draft retained in Chrome',
    focused: true,
    selectionStart: 7,
  })
  assert.deepEqual(result.candidateChange, {
    candidateRetained: true,
    diagramsRetained: true,
  })
  assert.deepEqual(result.runtimeChange, {
    candidateRetained: true,
    diagramsRebuilt: true,
  })
  assert.equal(exceptions.length, 0, JSON.stringify(exceptions))
})
