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
  'control-plane-client.js',
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
const organizationScope = {
  kind: 'organization',
  organizationId: 'org_00000000000000000000000001',
}

function sessionWith(scopes) {
  return {
    schemaVersion,
    expiresAt: '2099-09-02T00:00:00.000Z',
    actor,
    authorizedScopes: scopes,
  }
}

const areaByQuery = Object.freeze({
  'enterprise.organization.list': 'organization',
  'enterprise.membership.list': 'members',
  'enterprise.project.list': 'projects',
  'enterprise.policy.list': 'policy',
  'enterprise.fleet.list': 'fleet',
  'enterprise.usage.list': 'usage',
  'enterprise.audit.list': 'audit',
  'enterprise.integration.list': 'integration',
})

const kindByArea = Object.freeze({
  organization: 'enterprise_organization_page',
  members: 'enterprise_membership_page',
  projects: 'enterprise_project_repository_page',
  policy: 'enterprise_policy_page',
  fleet: 'enterprise_fleet_page',
  usage: 'enterprise_usage_page',
  audit: 'enterprise_audit_page',
  integration: 'enterprise_integration_page',
})

/** Deterministic facade fake covering session, product, and enterprise reads. */
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

test('personal deployment hides the Enterprise entry and keeps product areas navigable', async () => {
  const fixture = await restoredFixture('#/chat', facadeFake())
  await waitFor(() => navigationLinks(fixture.rootElement).chat !== undefined, 'navigation')
  await waitFor(() => Object.values(navigationLinks(fixture.rootElement)).length === 4, 'trimmed navigation')

  const links = navigationLinks(fixture.rootElement)
  assert.deepEqual(
    Object.keys(links).sort(),
    ['approvals', 'chat', 'settings', 'strongflow'],
  )
  assert.equal(links.chat.getAttribute('aria-disabled'), null)
  fixture.application.close()
})

test('enterprise deployment shows every entry including Enterprise', async () => {
  const fixture = await restoredFixture(
    '#/chat',
    facadeFake(sessionWith([organizationScope, repositoryScope])),
  )
  await waitFor(() => Object.values(navigationLinks(fixture.rootElement)).length === 5, 'full navigation')

  const links = navigationLinks(fixture.rootElement)
  assert.equal(links.enterprise.getAttribute('aria-disabled'), null)
  fixture.application.close()
})

test('directly opening an unauthorized Enterprise URL is refused by the page, not by trust', async () => {
  const client = facadeFake(sessionWith([repositoryScope]))
  const fixture = await restoredFixture('#/enterprise/resources', client)
  await waitFor(
    () => descendants(fixture.rootElement).some(node => (
      node.className === 'wwc-surface-route-denied'
    )),
    'enterprise route denial',
  )
  assert.equal(
    fixture.client.queries.some(query => areaByQuery[query.query] !== undefined),
    false,
    'no enterprise query left the browser for an unauthorized scope',
  )
  assert.equal(fixture.application.activeSurface.id, 'enterprise')
  assert.equal(navigationLinks(fixture.rootElement).enterprise, undefined)
  await assert.rejects(
    fixture.application.controlPlane.command({
      schemaVersion,
      requestId: 'req_00000000000000000000000001',
      actor,
      scope: repositoryScope,
      command: 'enterprise.organization.update',
      expectedRevision: 1,
      payload: {},
    }),
    error => error instanceof ControlPlaneClientError && error.kind === 'authorization',
  )
  fixture.application.close()
})

test('revoking the session hides navigation and exits the route with subscriptions closed', async () => {
  const client = facadeFake(sessionWith([organizationScope, repositoryScope]))
  const fixture = await restoredFixture('#/enterprise/resources', client)
  await waitFor(() => fixture.client.subscriptions.length > 0, 'enterprise subscription')
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

test('losing the enterprise scope mid-session returns to the safe entry', async () => {
  const client = facadeFake(sessionWith([organizationScope, repositoryScope]))
  const fixture = await restoredFixture('#/enterprise/resources', client)
  await waitFor(() => fixture.client.subscriptions.length > 0, 'enterprise subscription')
  const subscription = fixture.client.subscriptions[0]

  const personal = sessionWith([repositoryScope])
  Object.assign(fixture.application.authSession, {})
  client.restore = async () => structuredClone(personal)
  await fixture.application.authSession.restore()
  await waitFor(
    () => descendants(fixture.rootElement).some(node => (
      node.className === 'wwc-surface-route-denied'
    )),
    'enterprise route after scope loss',
  )
  await waitFor(() => subscription.handle.closed === true, 'closed enterprise subscription')
  assert.equal(navigationLinks(fixture.rootElement).enterprise, undefined)
  assert.equal(
    descendants(fixture.rootElement).some(node => (
      node.className === 'wwc-surface-route-safe-entry' && node.href === '#/chat'
    )),
    true,
  )
  fixture.application.close()
})

test('disabled navigation entries stay visible and block navigation', async () => {
  const client = facadeFake(sessionWith([organizationScope, repositoryScope]))
  const fixture = mountedFixture('#/chat', client, {
    navigationCapabilities: {
      deployment: 'enterprise',
      surfaceAccess: { enterprise: 'denied' },
    },
  })
  await waitFor(
    () => fixture.application.authSession.state.status === 'signed-in',
    'restored session',
  )
  await waitFor(() => Object.values(navigationLinks(fixture.rootElement)).length === 5, 'full navigation')

  const links = navigationLinks(fixture.rootElement)
  assert.equal(links.enterprise.getAttribute('aria-disabled'), 'true')
  assert.equal(links.enterprise.tabIndex, -1)
  assert.match(links.enterprise.textContent, /unavailable/iu)
  const event = {
    type: 'click',
    defaultPrevented: false,
    preventDefault() { this.defaultPrevented = true },
  }
  links.enterprise.dispatchEvent(event)
  assert.equal(event.defaultPrevented, true)
  fixture.application.close()
})

test('read-only navigation stays enterable and names its access level', async () => {
  const client = facadeFake(sessionWith([organizationScope, repositoryScope]))
  const fixture = mountedFixture('#/chat', client, {
    navigationCapabilities: {
      deployment: 'enterprise',
      surfaceAccess: { enterprise: 'read-only' },
    },
  })
  await waitFor(
    () => fixture.application.authSession.state.status === 'signed-in',
    'restored session',
  )
  await waitFor(() => navigationLinks(fixture.rootElement).enterprise !== undefined, 'read-only entry')
  const enterprise = navigationLinks(fixture.rootElement).enterprise
  assert.equal(enterprise.dataset.capability, 'read-only')
  assert.equal(enterprise.getAttribute('aria-disabled'), null)
  assert.match(enterprise.textContent, /read only/iu)
  fixture.application.close()
})

test('navigation shell keeps one facade and no direct network path', () => {
  const application = readFileSync(resolve(root, 'apps/client/src/application.ts'), 'utf8')
  assert.match(application, /import\('\.\/navigation-capability\.js'\)/u)
  assert.equal((application.match(/createControlPlaneClient/gu) ?? []).length, 2)
  assert.doesNotMatch(application, /\bfetch\s*\(|new\s+WebSocket/u)
})
