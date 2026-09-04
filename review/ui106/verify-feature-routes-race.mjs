// UI-106 review repro: tests/client-feature-routes-browser.test.mjs:88-94 is flaky (~5%).
// open('#/settings') only waits for the fixture global function to exist
// (waitForGlobal checks `typeof globalThis.inspectFeatureRoute === "function"`).
// The Settings page is mounted asynchronously: hashchange -> render() ->
// renderSettings() awaits auth restore + dynamic import before mounting.
// The very next statement calls inspectManagementPresentation('settings'),
// which synchronously queries '.wwc-settings' — racing the mount.
// Every later step in the same test uses settled()/waitFor on the selector and
// never flakes. Demonstrate the ordering gap with the same fixture modules in Node:
import assert from 'node:assert/strict'
import { mkdtempSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import {
  certificate, chromeBinary, closeServer, command, DevTools,
  evaluate, freePort, listen, staticClientServer, stopChild, waitForGlobal,
} from '/Volumes/ORICO/winwincode-worktrees/ui-workbench-xhigh/tests/fixtures/real-browser-harness.mjs'

const root = resolve('/Volumes/ORICO/winwincode-worktrees/ui-workbench-xhigh')
command(root, 'corepack', ['pnpm', '--filter', '@winwincode/client', 'build'])
const directory = mkdtempSync(join(tmpdir(), 'ui106-race-'))
const certificateFiles = certificate(root, directory)
const clientServer = staticClientServer({
  root, certificateFiles,
  fixturePath: 'tests/fixtures/browser-client-feature-routes.mjs',
  configuration: () => ({}),
})
const clientPort = await listen(clientServer)
const launched = await DevTools.launch({
  chromePath: chromeBinary(),
  directory,
  debugPort: await freePort(),
})
const { targetId } = await launched.devtools.send('Target.createTarget', { url: 'about:blank' })
const { sessionId } = await launched.devtools.send('Target.attachToTarget', { targetId, flatten: true })
await launched.devtools.send('Runtime.enable', {}, sessionId)
await launched.devtools.send('Page.enable', {}, sessionId)

// Exact sequence of the flaky test: operations route mounted, then open('#/settings')
// and immediately inspect — racing renderSettings' async mount.
await launched.devtools.send('Page.navigate', {
  url: `https://client.localhost:${String(clientPort)}/#/settings`,
}, sessionId)
await waitForGlobal(launched.devtools, sessionId, 'runFeatureNavigationScenario')
await evaluate(launched.devtools, sessionId, 'globalThis.runFeatureNavigationScenario()')
let gapObserved = 0
const runs = 120
for (let i = 0; i < runs; i += 1) {
  await launched.devtools.send('Page.navigate', {
    url: `https://client.localhost:${String(clientPort)}/#/settings/runtime?fixture=cycle${String(i)}`,
  }, sessionId)
  await evaluate(launched.devtools, sessionId, 'globalThis.inspectFeatureRoute("operations")')
  await launched.devtools.send('Page.navigate', {
    url: `https://client.localhost:${String(clientPort)}/#/settings`,
  }, sessionId)
  const immediate = await evaluate(
    launched.devtools, sessionId, 'globalThis.inspectManagementPresentation("settings")',
  )
  if (immediate.page === null) gapObserved += 1
}
console.log(`immediate inspect after waitForGlobal: ${gapObserved}/${runs} runs observed page=null`)
if (gapObserved === 0) { console.log(`NOTE: 0/${runs} observed this run — gap is load-dependent; original test failed 3/55 times under parallel load`); process.exit(0) }
console.log('CONFIRMED: synchronous inspection races the async Settings mount')

launched.devtools.close()
await stopChild(launched.chrome, 'SIGTERM')
await closeServer(clientServer)
rmSync(directory, { recursive: true, force: true })
