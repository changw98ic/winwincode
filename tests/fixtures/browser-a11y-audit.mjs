// UI-604 keyboard, focus, landmark, and live-region audit fixture.
//
// The shell findings cannot be pinned by a deterministic DOM double because
// they are about real focus order, real landmark roles, and the real set of
// live regions a page exposes after a realtime render.  This fixture mounts the
// one browser shell against a closed facade and reports those facts for every
// management surface, plus the chat surface.

import { mountWinWinCodeClient } from '/module/application.js'

const schemaVersion = 'winwincode/v1'
const actor = { kind: 'user', id: 'usr_00000000000000000000000001' }
const scope = {
  kind: 'repository',
  organizationId: 'org_00000000000000000000000001',
  workspaceId: 'wsp_00000000000000000000000001',
  projectId: 'prj_00000000000000000000000001',
  repositoryId: 'rep_00000000000000000000000001',
}
const productSessionId = 'psn_00000000000000000000000001'
const browserSession = {
  schemaVersion,
  expiresAt: '2099-09-02T00:00:00.000Z',
  actor,
  authorizedScopes: [scope],
}

function page() {
  return { hasMore: false, nextCursor: null }
}

function response(request, result) {
  return {
    schemaVersion,
    requestId: request.requestId,
    query: request.query,
    result,
    page: page(),
  }
}

function session() {
  return {
    id: productSessionId,
    projectId: scope.projectId,
    repositoryId: scope.repositoryId,
    revision: 1,
    state: 'idle',
    title: 'Audit fixture Chat',
    updatedAt: '2026-09-02T00:00:00.000Z',
  }
}

const controlPlane = {
  serverUrl: 'https://control.localhost',
  async restore() { return structuredClone(browserSession) },
  async login() { return structuredClone(browserSession) },
  async logout() {},
  async query(request) {
    if (request.query === 'settings.get') {
      return response(request, {
        revision: 1,
        defaultModelRoute: null,
        workerConcurrencyLimit: 2,
      })
    }
    if (request.query === 'credential.reference.list') {
      return response(request, { kind: 'credential_reference_page', items: [] })
    }
    if (request.query === 'worker.list') {
      return response(request, { kind: 'worker_page', items: [] })
    }
    if (request.query === 'delivery.list') {
      return response(request, { kind: 'delivery_page', items: [] })
    }
    if (request.query === 'session.list') {
      return response(request, { kind: 'product_session_page', items: [] })
    }
    if (request.query === 'session.get') return response(request, session())
    if (request.query === 'session.interactions.list') {
      return response(request, { kind: 'chat_interaction_page', items: [] })
    }
    if (request.query === 'approval.list') {
      return response(request, { kind: 'approval_page', items: [] })
    }
    throw new Error(`unexpected query: ${request.query}`)
  },
  async command(request) {
    throw new Error(`unexpected command: ${request.command}`)
  },
  subscribe(options) {
    return {
      cursor: null,
      resume() {},
      reconnect() {},
      close() { options },
    }
  },
  close() {},
}

const root = document.querySelector('[data-winwincode-client-root]')
mountWinWinCodeClient({
  root,
  serverUrl: controlPlane.serverUrl,
  controlPlane,
  copyText() {},
})

async function waitFor(predicate, label) {
  const deadline = Date.now() + 5_000
  while (Date.now() < deadline) {
    if (predicate()) return
    await new Promise(resolve => { setTimeout(resolve, 20) })
  }
  throw new Error(`timed out waiting for ${label}: ${document.body.textContent.slice(0, 400)}`)
}

const PAGE_SELECTOR = {
  chat: '.wwc-chat',
  settings: '.wwc-settings',
  operations: '.wwc-local-operations',
  attention: '.wwc-attention-center',
  decisions: '.wwc-local-decisions',
}

// UI-604: the audit keeps one explicit allow-list of live regions.  Anything
// outside it — a list, a card grid, a whole page container — is a regression
// toward re-announcing every realtime render.
const LIVE_REGION_ALLOWLIST = Object.freeze([
  // shell
  'wwc-connection-status',
  'wwc-connection-copy-feedback',
  'wwc-auth-session-status',
  'wwc-scope-selector-status',
  'wwc-readiness-summary',
  'wwc-client-error-copy-feedback',
  'wwc-enterprise-route-status',
  // chat
  'wwc-chat-status',
  'wwc-chat-model-notice',
  'wwc-chat-error',
  'wwc-chat-messages',
  'wwc-chat-convert-error',
  // management surfaces: exactly one polite status line per page
  'wwc-settings-status',
  'wwc-local-operations-status',
  'wwc-local-resource-status',
  'wwc-attention-center-status',
  'wwc-local-decisions-status',
])

function classNameOf(node) {
  return typeof node.className === 'string' ? node.className : ''
}

function liveRegions() {
  return [...document.querySelectorAll('[aria-live]')].map(node => ({
    tag: node.tagName,
    role: node.getAttribute('role'),
    ariaLive: node.getAttribute('aria-live'),
    className: classNameOf(node),
  }))
}

function headings() {
  const tags = ['H1', 'H2', 'H3', 'H4', 'H5', 'H6']
  const list = [...document.querySelectorAll('h1, h2, h3, h4, h5, h6')].map(node => ({
    tag: node.tagName,
    level: tags.indexOf(node.tagName) + 1,
    text: node.textContent.trim(),
  }))
  const skipped = []
  for (let index = 1; index < list.length; index += 1) {
    const previous = list[index - 1].level
    const current = list[index].level
    // Nesting deeper is fine; moving back up past a parent is what breaks the
    // outline a screen reader builds for the page.
    if (current > previous + 1) skipped.push([list[index - 1].tag, list[index].tag])
  }
  return { list, skipped }
}

function landmarks() {
  const banner = document.querySelector('header')
  return {
    main: document.querySelectorAll('main').length,
    banner: banner === null ? 0 : 1,
    navigation: document.querySelectorAll('nav').length,
    navigationLabel: document.querySelector('nav')?.getAttribute('aria-label') ?? null,
    regions: document.querySelectorAll('[role="region"]').length,
  }
}

const STATUS_SELECTOR = {
  chat: '.wwc-chat-status',
  settings: '.wwc-settings-status .wwc-status-badge-label',
  operations: '.wwc-local-operations-status .wwc-status-badge-label',
  attention: '.wwc-attention-center-status .wwc-status-badge-label',
  decisions: '.wwc-local-decisions-status .wwc-status-badge-label',
}

async function settled(name) {
  const selector = PAGE_SELECTOR[name]
  await waitFor(() => document.querySelector(selector) !== null, selector)
  const statusSelector = STATUS_SELECTOR[name]
  await waitFor(() => {
    const label = document.querySelector(statusSelector)?.textContent ?? ''
    return label.length > 0 && !/^Loading|^Updating/u.test(label)
  }, `${selector} ${statusSelector}`)
}

globalThis.inspectAccessibility = async name => {
  const selector = PAGE_SELECTOR[name]
  if (selector === undefined) throw new Error(`unknown surface: ${name}`)
  await settled(name)

  const skipLink = document.querySelector('.wwc-skip-link')
  const firstFocusable = [...document.querySelectorAll(
    'a[href], button:not([disabled]), input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])',
  )].filter(node => node.offsetParent !== null || node === skipLink)
  const main = document.querySelector('main')

  return {
    surface: name,
    hash: location.hash,
    headings: headings().list,
    skippedHeadingLevels: headings().skipped,
    h1: [...document.querySelectorAll('main h1')].map(node => node.textContent.trim()),
    landmarks: landmarks(),
    liveRegions: liveRegions(),
    unexpectedLiveRegions: liveRegions().filter(region => (
      !LIVE_REGION_ALLOWLIST.some(allowed => region.className.split(' ').includes(allowed))
    )),
    surfaceSlotLive: document.querySelector('.wwc-surface-slot')?.getAttribute('aria-live') ?? null,
    collectionLiveRegions: [...document.querySelectorAll(
      '[aria-live].wwc-attention-center-list,'
      + ' [aria-live].wwc-settings-credential-list,'
      + ' [aria-live].wwc-local-worker-list,'
      + ' [aria-live].wwc-local-input-list,'
      + ' [aria-live].wwc-local-approval-list,'
      + ' [aria-live].wwc-local-attention-list,'
      + ' [aria-live][class*="-list"],'
      + ' [aria-live][class*="-card"]',
    )].map(node => classNameOf(node)),
    skipLink: {
      present: skipLink !== null,
      firstFocusable: firstFocusable[0] === skipLink,
      targetHiddenUntilFocus: skipLink === null
        ? null
        : getComputedStyle(skipLink).clipPath !== 'none',
      label: skipLink?.textContent ?? null,
    },
    mainFocusable: main?.tabIndex === -1,
    noHorizontalOverflow: document.documentElement.scrollWidth
      <= document.documentElement.clientWidth,
  }
}

globalThis.runSkipLinkScenario = async () => {
  await settled('settings')
  const skipLink = document.querySelector('.wwc-skip-link')
  const main = document.querySelector('main')
  const beforeHash = location.hash
  const hiddenClip = getComputedStyle(skipLink).clipPath

  skipLink.focus()
  const focusedClip = getComputedStyle(skipLink).clipPath
  skipLink.click()
  return {
    beforeHash,
    afterHash: location.hash,
    hiddenClip,
    focusedClip,
    focusAfterActivation: document.activeElement === main ? 'main' : classNameOf(document.activeElement),
    mainTag: main.tagName,
    mainTabIndex: main.tabIndex,
  }
}

globalThis.inspectHeadingLevels = async name => {
  await settled(name)
  // Scoped to the page root so the shell's Scope selector and readiness
  // sections do not blur the page's own outline.
  const root = document.querySelector(PAGE_SELECTOR[name])
  return [...root.querySelectorAll('h1, h2, h3, h4')].map(node => ({
    tag: node.tagName,
    text: node.textContent.trim(),
  }))
}
