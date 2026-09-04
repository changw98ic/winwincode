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
const identity = 'org_00000000000000000000000001'
const workspaceId = 'wsp_00000000000000000000000001'
const projectId = 'prj_00000000000000000000000001'
const repositoryOne = 'rep_00000000000000000000000001'
const repositoryTwo = 'rep_00000000000000000000000002'
const repositoryThree = 'rep_00000000000000000000000003'
const SECRET_MARKER = 'vault-locator-secret-marker'

test('a real browser opens the Attention-first Home dashboard as the first screen', async t => {
  const chromePath = chromeBinary()
  assert.notEqual(chromePath, null, 'Chrome or Chromium is required for the Home browser test')
  command(root, 'corepack', ['pnpm', '--filter', '@winwincode/client', 'build'])
  const directory = mkdtempSync(join(tmpdir(), 'winwincode-home-dashboard-'))
  const certificateFiles = certificate(root, directory)
  const clientServer = staticClientServer({
    root,
    certificateFiles,
    fixturePath: 'tests/fixtures/browser-home-dashboard.mjs',
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
  const evaluateInBrowser = async expression => evaluate(devtools, sessionId, expression)

  // The start-up route carries no product path at all, and this identity has
  // several authorized repository Scopes: the shell must ask for an exact Scope
  // instead of opening an arbitrary Chat or the first Delivery.
  await devtools.send('Page.navigate', { url: clientOrigin }, sessionId)
  await waitForGlobal(devtools, sessionId, 'homeReady')
  await waitForGlobal(devtools, sessionId, 'inspectLanding')
  const landing = await evaluateInBrowser('globalThis.inspectLanding()')
  assert.equal(landing.surface, 'home', JSON.stringify(landing))
  assert.equal(landing.hash === '' || landing.hash === '#/home', true, landing.hash)
  assert.equal(landing.present, false, 'no dashboard mounts without an exact Scope')
  assert.equal(landing.leak, false)
  const landingText = await evaluateInBrowser(
    "document.body.textContent.replace(/\\s+/gu, ' ')",
  )
  assert.match(landingText, /Choose an authorized repository Scope/u)
  assert.doesNotMatch(landingText, /Conversation workspace/u)

  // With one exact Scope in the URL, the dashboard is the first screen.
  const home = await evaluateInBrowser(`globalThis.openHome('#/home?organizationId=${identity}`
    + `&workspaceId=${workspaceId}&projectId=${projectId}&repositoryId=${repositoryOne}')`)
  assert.equal(home.liveRegions, 1, 'the dashboard keeps exactly one polite live region')
  assert.match(home.status, /Ready ·/u)
  assert.deepEqual(home.sections.map(section => section.id), [
    'decisions',
    'active',
    'failing',
    'completed',
    'visited',
  ])
  const sectionOf = dashboard => id => dashboard.sections.find(
    candidate => candidate.id === id,
  )
  const section = sectionOf(home)
  assert.equal(section('decisions').cards.length, 2, JSON.stringify(home.sections))
  assert.equal(section('active').cards.length, 1)
  assert.equal(section('failing').cards.length, 1)
  assert.equal(section('completed').cards.length, 1)
  assert.equal(section('visited').cards.length, 0)
  assert.equal(section('decisions').cards[0].title, 'Review the proposed delivery scope')
  assert.match(section('active').cards[0].title, /repository 1/u)
  assert.equal(home.firstUse.hidden, true)
  assert.equal(home.usage.present, true)
  assert.deepEqual(home.unavailableNotes, [])

  // Every card opens its exact, Scope-complete deep link.
  const scoped = `organizationId=${identity}&workspaceId=${workspaceId}`
    + `&projectId=${projectId}&repositoryId=${repositoryOne}`
  for (const action of home.actions) {
    assert.match(action.href, new RegExp(`repositoryId=${repositoryOne}$`), action.href)
    assert.equal(action.disabled, null, action.href)
  }
  assert.equal(
    home.actions.filter(action => action.href === `#/attention?session=psn_00000000000000000000000001&${scoped}`).length,
    1,
    JSON.stringify(home.actions),
  )
  assert.equal(
    home.actions.filter(action => action.href === `#/strongflow?delivery=dlv_00000000000000000000000001`
      + `&stageRun=str_00000000000000000000000001&view=unified&${scoped}`).length,
    2,
    'the in-progress and the failing card open the exact StrongFlow StageRun',
  )
  assert.equal(
    home.chatLinks.filter(href => href === `#/chat?session=psn_00000000000000000000000001&${scoped}`).length,
    1,
    JSON.stringify(home.chatLinks),
  )

  // No visit exists yet, because no Delivery has been opened in this browser.
  const visitsBefore = await evaluateInBrowser('globalThis.readRecentVisits()')
  assert.equal(visitsBefore.stored, false, JSON.stringify(visitsBefore))

  // Opening a card records one browser-local visit and shows the Delivery in the
  // "recently opened" section when the user comes back.
  await evaluateInBrowser("document.querySelector('.wwc-home-card-action').click()")
  await evaluateInBrowser('globalThis.waitUntil(() => '
    + 'globalThis.readRecentVisits().entries.length === 1)')
  const visits = await evaluateInBrowser('globalThis.readRecentVisits()')
  assert.equal(visits.entries.length, 1, JSON.stringify(visits))
  assert.deepEqual(Object.keys(visits.entries[0]).sort(), ['at', 'deliveryId', 'kind', 'scope'])
  assert.equal(visits.entries[0].deliveryId, 'dlv_00000000000000000000000001')
  assert.equal(JSON.stringify(visits).includes(SECRET_MARKER), false)
  const homeAgain = await evaluateInBrowser(`globalThis.openHome('#/home?organizationId=${identity}`
    + `&workspaceId=${workspaceId}&projectId=${projectId}&repositoryId=${repositoryOne}')`)
  assert.equal(homeAgain.liveRegions, 1)
  assert.equal(sectionOf(homeAgain)('visited').cards.length, 1, JSON.stringify(homeAgain.sections))
  assert.match(sectionOf(homeAgain)('visited').cards[0].title, /repository 1/u)

  // A Scope switch re-reads every projection and re-renders in isolation.
  const switched = await evaluateInBrowser('globalThis.switchRepositoryScope()')
  // Every projection re-read the new Scope after the switch.
  assert.deepEqual(switched.scopedQueries.slice(-2), [repositoryTwo, repositoryTwo])
  assert.ok(switched.scopedQueries.includes(repositoryOne), switched.scopedQueries.join(' '))
  assert.match(JSON.stringify(switched.afterSectionTitles), /repository 2/u)
  assert.doesNotMatch(JSON.stringify(switched.afterSectionTitles), /repository 1/u)
  assert.equal(switched.leak, false)
  const switchedDashboard = await evaluateInBrowser('globalThis.readDashboard()')
  assert.match(switchedDashboard.status, /Ready ·/u)
  assert.equal(
    switchedDashboard.actions.every(action => (action.href ?? '').includes(repositoryTwo)),
    true,
    JSON.stringify(switchedDashboard.actions),
  )
  assert.equal(sectionOf(switchedDashboard)('visited').cards.length, 0)
  assert.equal(switchedDashboard.liveRegions, 1)

  // A Scope that was never used offers the explicit first-use entry points.
  const firstUse = await evaluateInBrowser(`globalThis.openHome('#/home?organizationId=${identity}`
    + `&workspaceId=${workspaceId}&projectId=${projectId}&repositoryId=${repositoryThree}')`)
  assert.equal(firstUse.firstUse.hidden, false, JSON.stringify(firstUse.firstUse))
  assert.deepEqual(firstUse.firstUse.links, [
    `#/strongflow?organizationId=${identity}&workspaceId=${workspaceId}`
      + `&projectId=${projectId}&repositoryId=${repositoryThree}`,
    `#/chat?organizationId=${identity}&workspaceId=${workspaceId}`
      + `&projectId=${projectId}&repositoryId=${repositoryThree}`,
  ])
  assert.equal(firstUse.actions.length, 0)
  assert.equal(firstUse.leak, false)
})
