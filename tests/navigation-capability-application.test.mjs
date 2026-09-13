import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { pathToFileURL } from 'node:url'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')
const compiler = spawnSync(
  'corepack',
  [
    'pnpm',
    'exec',
    'tsc',
    '-p',
    'apps/client/tsconfig.navigation-capability-application-tests.json',
    '--pretty',
    'false',
    '--incremental',
    'false',
  ],
  { cwd: root, encoding: 'utf8' },
)
assert.equal(
  compiler.status,
  0,
  `Navigation application boundary did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const cacheRoot = resolve(root, '.cache/navigation-capability-application-tests')
const applicationModule = await import(`${pathToFileURL(resolve(
  cacheRoot,
  'application.js',
)).href}`)
const facadeModule = await import(`${pathToFileURL(resolve(
  cacheRoot,
  'community-control-plane-client.js',
)).href}`)
const { mountWinWinCodeClient } = applicationModule
const { ControlPlaneClientError } = facadeModule

const schemaVersion = 'winwincode/v1'
const actor = { kind: 'user', id: 'usr_00000000000000000000000001' }
const repositoryScope = {
  kind: 'repository',
  organizationId: 'org_00000000000000000000000001',
  workspaceId: 'wsp_00000000000000000000000001',
  projectId: 'prj_00000000000000000000000001',
  repositoryId: 'rep_00000000000000000000000001',
}
function sessionWith(scopes) {
  return {
    schemaVersion,
    expiresAt: '2099-09-02T00:00:00.000Z',
    actor,
    authorizedScopes: scopes,
  }
}

const areaByQuery = Object.freeze({})

const kindByArea = Object.freeze({})

/** Deterministic facade fake covering session and Community product reads. */
function facadeFake(currentSession = sessionWith([repositoryScope])) {
  const queries = []
  const commands = []
  const subscriptions = []
  let deniedAreas = new Set()
  let closed = false
  const productSession = {
    id: 'psn_00000000000000000000000001',
    projectId: repositoryScope.projectId,
    repositoryId: repositoryScope.repositoryId,
    revision: 1,
    state: 'idle',
    title: 'Navigation fixture Chat',
    updatedAt: '2026-09-02T00:00:00.000Z',
  }
  function respond(request, result) {
    return {
      schemaVersion,
      requestId: request.requestId,
      query: request.query,
      result,
      page: { hasMore: false, nextCursor: null },
    }
  }
  const client = {
    queries,
    commands,
    subscriptions,
    serverUrl: 'https://control.example/navigation',
    deniedAreas,
    async restore() { return structuredClone(currentSession) },
    async login() { return structuredClone(currentSession) },
    async logout() {},
    async query(request) {
      queries.push(structuredClone(request))
      const area = areaByQuery[request.query]
      if (area !== undefined && deniedAreas.has(area)) throw new ControlPlaneClientError({
        kind: 'authorization',
        code: 'PERMISSION_DENIED',
        message: 'private navigation diagnostics',
        requestId: request.requestId,
        retryable: false,
      })
      switch (request.query) {
        case 'session.list':
          return respond(request, { kind: 'product_session_page', items: [productSession] })
        case 'session.get':
          return respond(request, productSession)
        case 'session.messages.list':
          return respond(request, { kind: 'chat_message_page', items: [] })
        case 'settings.get':
          return respond(request, {
            revision: 1,
            defaultModelRoute: null,
            workerConcurrencyLimit: 2,
          })
        case 'runtime.projection.get':
          return respond(request, {
            kind: 'runtime_projection',
            deliveryId: null,
            stageRunId: null,
            readCursor: null,
            eventCursor: {
              eventId: null,
              sequence: 0,
              scope: repositoryScope,
              stream: {
                kind: 'product-session',
                productSessionId: productSession.id,
              },
            },
            lastProjectionSequence: 0,
            productSessionId: productSession.id,
            rebuiltAt: '2026-09-02T00:00:00.000Z',
            revision: 1,
            sessions: [{
              productSessionId: productSession.id,
              activities: [],
              agentEdges: [],
              agents: [],
              asOfSequence: 0,
              attempt: 1,
              codexThreadId: 'ctx_00000000000000000000000001',
              deliveryTaskId: null,
              diffSummary: null,
            }],
          })
        case 'session.interactions.list':
          return respond(request, { kind: 'chat_interaction_page', items: [] })
        case 'approval.list':
          return respond(request, { kind: 'approval_page', items: [] })
        case 'delivery.list':
          return respond(request, { kind: 'delivery_page', items: [] })
        case 'model.route.availability.list':
          return respond(request, {
            kind: 'model_route_availability_page',
            defaultModelId: null,
            defaultProviderId: null,
            items: [],
            reason: 'no_provider',
            requestPoolRevision: 1,
            requestPoolSource: {
              kind: 'project',
              organizationId: repositoryScope.organizationId,
              workspaceId: repositoryScope.workspaceId,
              projectId: repositoryScope.projectId,
            },
            scope: repositoryScope,
            settingsRevision: null,
            settingsSource: null,
            status: 'disabled',
          })
        default:
          if (area !== undefined) {
            return respond(request, {
              kind: kindByArea[area],
              snapshotRevision: 4,
              items: [],
            })
          }
          throw new Error(`unexpected query: ${request.query}`)
      }
    },
    async command(request) {
      commands.push(structuredClone(request))
      throw new ControlPlaneClientError({
        kind: 'authorization',
        code: 'PERMISSION_DENIED',
        message: 'Server rejected the command',
        requestId: request.requestId,
        retryable: false,
      })
    },
    subscribe(options) {
      const handle = {
        cursor: null,
        closed: false,
        resume() {},
        reconnect() {},
        close() { this.closed = true },
      }
      subscriptions.push({ options, handle })
      return handle
    },
    close() { closed = true },
  }
  return client
}

class FakeElement {
  constructor(ownerDocument, tagName) {
    this.ownerDocument = ownerDocument
    this.tagName = tagName.toUpperCase()
  }

  attributes = new Map()
  children = []
  listeners = new Map()
  dataset = {}
  className = ''
  disabled = false
  hidden = false
  tabIndex = 0
  href = ''
  #textContent = ''

  get textContent() { return this.#textContent }

  set textContent(value) {
    this.#textContent = String(value)
    this.children = []
  }

  append(...children) { this.children.push(...children) }

  replaceChildren(...children) { this.children = [...children] }

  setAttribute(name, value) { this.attributes.set(name, String(value)) }

  getAttribute(name) { return this.attributes.get(name) ?? null }

  removeAttribute(name) { this.attributes.delete(name) }

  addEventListener(name, listener) {
    const current = this.listeners.get(name) ?? []
    current.push(listener)
    this.listeners.set(name, current)
  }

  removeEventListener(name, listener) {
    const current = this.listeners.get(name) ?? []
    this.listeners.set(name, current.filter(candidate => candidate !== listener))
  }

  dispatchEvent(event) {
    for (const listener of this.listeners.get(event.type) ?? []) listener(event)
    return !event.defaultPrevented
  }

  click() {
    let defaultPrevented = false
    this.dispatchEvent({
      type: 'click',
      get defaultPrevented() { return defaultPrevented },
      preventDefault() { defaultPrevented = true },
    })
  }
}

class FakeDocument {
  createElement(tagName) { return new FakeElement(this, tagName) }
}

class FakeWindow {
  location = { hash: '', pathname: '/', search: '' }
  history = { replaceState() {} }
  listeners = new Map()
  entropy = 0
  crypto = {
    getRandomValues: value => {
      this.entropy += 1
      value.fill(this.entropy)
      return value
    },
  }

  addEventListener(name, listener) {
    const current = this.listeners.get(name) ?? []
    current.push(listener)
    this.listeners.set(name, current)
  }

  removeEventListener(name, listener) {
    const current = this.listeners.get(name) ?? []
    this.listeners.set(name, current.filter(candidate => candidate !== listener))
  }

  dispatch(name) {
    for (const listener of this.listeners.get(name) ?? []) listener()
  }
}

function descendants(node) {
  return [node, ...node.children.flatMap(child => descendants(child))]
}

function navigationLinks(rootElement) {
  return Object.fromEntries(
    descendants(rootElement)
      .filter(node => node.className === 'wwc-navigation-link')
      .map(node => [node.dataset.surface, node]),
  )
}

async function waitFor(predicate, label) {
  const deadline = Date.now() + 5_000
  while (Date.now() < deadline) {
    if (predicate()) return
    await new Promise(resolvePromise => { setTimeout(resolvePromise, 10) })
  }
  assert.fail(`timed out waiting for ${label}`)
}

function mountedFixture(hash, client = facadeFake(), applicationOptions = {}) {
  const document = new FakeDocument()
  const rootElement = new FakeElement(document, 'div')
  const browser = new FakeWindow()
  browser.location.hash = hash
  const application = mountWinWinCodeClient({
    root: rootElement,
    serverUrl: client.serverUrl,
    window: browser,
    controlPlane: client,
    ...applicationOptions,
  })
  return { application, browser, client, rootElement }
}

async function restoredFixture(hash, client = facadeFake()) {
  const fixture = mountedFixture(hash, client)
  await waitFor(
    () => fixture.application.authSession.state.status === 'signed-in',
    'restored session',
  )
  return fixture
}

test('personal deployment shows the five product entries without Enterprise', async () => {
  const fixture = await restoredFixture('#/chat', facadeFake())
  await waitFor(() => navigationLinks(fixture.rootElement).chat !== undefined, 'navigation')
  // UI-504: Home joins Chat, StrongFlow, Settings and Attention in the nav.
  await waitFor(() => Object.values(navigationLinks(fixture.rootElement)).length === 5, 'canonical navigation')

  const links = navigationLinks(fixture.rootElement)
  assert.deepEqual(
    Object.keys(links).sort(),
    ['attention', 'chat', 'home', 'settings', 'strongflow'],
  )
  assert.equal(links.chat.getAttribute('aria-disabled'), null)
  assert.equal(links.enterprise, undefined)
  fixture.application.close()
})

test('unknown Enterprise URLs fall back to Home instead of loading an Enterprise surface', async () => {
  const client = facadeFake(sessionWith([repositoryScope]))
  const fixture = await restoredFixture('#/enterprise/resources', client)
  await waitFor(
    () => navigationLinks(fixture.rootElement).home !== undefined,
    'home fallback navigation',
  )
  assert.equal(fixture.application.activeSurface.id, 'home')
  assert.equal(navigationLinks(fixture.rootElement).enterprise, undefined)
  fixture.application.close()
})

test('revoking the session hides navigation and exits the route with subscriptions closed', async () => {
  const client = facadeFake(sessionWith([repositoryScope]))
  const fixture = await restoredFixture('#/attention', client)
  await waitFor(() => fixture.client.subscriptions.length > 0, 'attention subscription')
  const subscription = fixture.client.subscriptions[0]

  fixture.application.authSession.authenticationRequired(new ControlPlaneClientError({
    kind: 'authentication',
    code: 'AUTHENTICATION_REQUIRED',
    message: 'private revoked navigation diagnostics',
    requestId: null,
    retryable: false,
  }))
  await waitFor(
    () => Object.values(navigationLinks(fixture.rootElement)).every(link => link.hidden),
    'hidden navigation after revocation',
  )
  await waitFor(() => subscription.handle.closed === true, 'closed subscription')
  assert.equal(fixture.application.authSession.state.status, 'authentication-required')
  fixture.application.close()
})

test('losing the repository Scope mid-session disables product entries', async () => {
  const client = facadeFake(sessionWith([repositoryScope]))
  const fixture = await restoredFixture('#/attention', client)
  await waitFor(() => navigationLinks(fixture.rootElement).attention !== undefined, 'attention nav')

  const empty = sessionWith([])
  client.restore = async () => structuredClone(empty)
  await fixture.application.authSession.restore()
  await waitFor(
    () => navigationLinks(fixture.rootElement).chat?.getAttribute('aria-disabled') === 'true'
      || navigationLinks(fixture.rootElement).chat === undefined,
    'repository scope loss',
  )
  fixture.application.close()
})

test('disabled navigation entries stay visible and block navigation', async () => {
  const client = facadeFake(sessionWith([repositoryScope]))
  const fixture = mountedFixture('#/chat', client, {
    navigationCapabilities: {
      surfaceAccess: { chat: 'denied' },
    },
  })
  await waitFor(
    () => fixture.application.authSession.state.status === 'signed-in',
    'restored session',
  )
  await waitFor(() => navigationLinks(fixture.rootElement).chat !== undefined, 'disabled entry')

  const links = navigationLinks(fixture.rootElement)
  assert.equal(links.chat.getAttribute('aria-disabled'), 'true')
  assert.equal(links.chat.tabIndex, -1)
  assert.match(links.chat.textContent, /unavailable/iu)
  const event = {
    type: 'click',
    defaultPrevented: false,
    preventDefault() { this.defaultPrevented = true },
  }
  links.chat.dispatchEvent(event)
  assert.equal(event.defaultPrevented, true)
  fixture.application.close()
})

test('read-only navigation stays enterable and names its access level', async () => {
  const client = facadeFake(sessionWith([repositoryScope]))
  const fixture = mountedFixture('#/chat', client, {
    navigationCapabilities: {
      surfaceAccess: { chat: 'read-only' },
    },
  })
  await waitFor(
    () => fixture.application.authSession.state.status === 'signed-in',
    'restored session',
  )
  await waitFor(() => navigationLinks(fixture.rootElement).chat !== undefined, 'read-only entry')
  const chat = navigationLinks(fixture.rootElement).chat
  assert.equal(chat.dataset.capability, 'read-only')
  assert.equal(chat.getAttribute('aria-disabled'), null)
  assert.match(chat.textContent, /read only/iu)
  fixture.application.close()
})

test('WebSocket authorization revocation closes the feature and shows the shell safe entry', async () => {
  const client = facadeFake(sessionWith([repositoryScope]))
  const fixture = await restoredFixture('#/attention', client)
  await waitFor(() => fixture.client.subscriptions.length > 0, 'attention subscription')
  const subscription = fixture.client.subscriptions[0]

  await subscription.options.onAuthorizationRevoked(null)

  await waitFor(() => subscription.handle.closed === true, 'revoked subscription cleanup')
  await waitFor(
    () => descendants(fixture.rootElement).some(node => (
      node.className === 'wwc-surface-route-safe-entry'
    )),
    'shell safe entry',
  )
  assert.equal(fixture.application.activeSurface.id, 'attention')
  assert.equal(navigationLinks(fixture.rootElement).attention.dataset.capability, 'available')
  assert.equal(navigationLinks(fixture.rootElement).attention.getAttribute('data-route-access'), 'denied')
  fixture.application.close()
})

test('navigation shell keeps one facade and no direct network path', () => {
  const application = readFileSync(resolve(root, 'apps/client/src/application.ts'), 'utf8')
  const navigation = readFileSync(resolve(root, 'apps/client/src/navigation-capability.ts'), 'utf8')
  assert.match(application, /from '\.\/navigation-capability\.js'/u)
  assert.match(navigation, /from '\.\/client-surface\.js'/u)
  assert.doesNotMatch(navigation, /from '\.\/application\.js'/u)
  assert.equal((application.match(/\bcreateControlPlaneClient\b/gu) ?? []).length, 2)
  assert.doesNotMatch(application, /\bfetch\s*\(|new\s+WebSocket/u)
})
