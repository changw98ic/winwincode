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
    'apps/client/tsconfig.enterprise-application-tests.json',
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
  `Enterprise application did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const cacheRoot = resolve(root, '.cache/enterprise-application-tests')
const applicationModule = await import(`${pathToFileURL(resolve(
  cacheRoot,
  'application.js',
)).href}`)
const facadeModule = await import(`${pathToFileURL(resolve(
  cacheRoot,
  'control-plane-client.js',
)).href}`)
const {
  clientSurfaceFromHash,
  mountWinWinCodeClient,
} = applicationModule
const { ControlPlaneClientError } = facadeModule

const schemaVersion = 'winwincode/v1'
const actor = { kind: 'user', id: 'usr_00000000000000000000000001' }
const scope = { kind: 'organization', organizationId: 'org_00000000000000000000000001' }
const session = {
  schemaVersion,
  expiresAt: '2026-08-28T00:00:00.000Z',
  actor,
  authorizedScopes: [scope],
}

function canonicalId(prefix, value) {
  return `${prefix}_${String(value).padStart(26, '0')}`
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

function enterpriseFacadeFake(currentSession = session, restoreError = null) {
  const queries = []
  const subscriptions = []
  const allowed = Object.fromEntries(Object.values(areaByQuery).map(area => [area, true]))
  const revisions = Object.fromEntries(Object.values(areaByQuery).map(area => [area, 4]))
  let closed = false
  return {
    queries,
    subscriptions,
    allowed,
    serverUrl: 'https://control.example/enterprise',
    async restore() {
      if (restoreError !== null) throw restoreError
      return structuredClone(currentSession)
    },
    async login() { return structuredClone(currentSession) },
    async logout() {},
    async query(request) {
      const area = areaByQuery[request.query]
      assert.notEqual(area, undefined)
      queries.push(structuredClone(request))
      if (!allowed[area]) throw new ControlPlaneClientError({
        kind: 'authorization',
        code: 'PERMISSION_DENIED',
        message: 'private permission diagnostics',
        requestId: request.requestId,
        retryable: false,
      })
      const firstAuditPage = area === 'audit' && request.page.cursor === null
      return {
        schemaVersion,
        requestId: request.requestId,
        query: request.query,
        result: {
          kind: kindByArea[area],
          snapshotRevision: revisions[area],
          items: [],
        },
        page: firstAuditPage
          ? { hasMore: true, nextCursor: 'audit_next' }
          : { hasMore: false, nextCursor: null },
      }
    },
    async command() { throw new Error('unexpected command') },
    subscribe(options) {
      const handle = {
        cursor: null,
        resumed: false,
        reconnected: false,
        closed: false,
        resume() { this.resumed = true },
        reconnect() { this.reconnected = true },
        close() { this.closed = true },
      }
      subscriptions.push({ options, handle })
      return handle
    },
    close() { closed = true },
    get closed() { return closed },
  }
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
  required = false
  selected = false
  tabIndex = 0
  title = ''
  type = ''
  value = ''
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

  dispatch(name) {
    let defaultPrevented = false
    const event = {
      type: name,
      get defaultPrevented() { return defaultPrevented },
      preventDefault() { defaultPrevented = true },
    }
    this.dispatchEvent(event)
    return event
  }
}

class FakeDocument {
  createElement(tagName) { return new FakeElement(this, tagName) }
}

class FakeWindow {
  location = { hash: '' }
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

function byClass(rootElement, className) {
  const match = descendants(rootElement).find(node => node.className === className)
  assert.notEqual(match, undefined, `missing .${className}`)
  return match
}

function visibleText(node) {
  return descendants(node).map(current => current.textContent).join(' ')
}

async function waitFor(predicate, label) {
  for (let attempt = 0; attempt < 300; attempt += 1) {
    if (predicate()) return
    await new Promise(resolvePromise => { setImmediate(resolvePromise) })
  }
  assert.fail(`timed out waiting for ${label}`)
}

function mountedFixture(hash, client = enterpriseFacadeFake()) {
  const document = new FakeDocument()
  const rootElement = new FakeElement(document, 'div')
  const browser = new FakeWindow()
  browser.location.hash = hash
  const application = mountWinWinCodeClient({
    root: rootElement,
    serverUrl: client.serverUrl,
    window: browser,
    controlPlane: client,
  })
  return { application, browser, client, rootElement }
}

test('enterprise deep link loads its split route through the one application facade and paginates', async () => {
  const fixture = mountedFixture('#/enterprise/operations?tab=usage')
  await waitFor(
    () => descendants(fixture.rootElement).some(node => (
      node.className === 'wwc-enterprise-operations'
    )),
    'enterprise operations deep link',
  )
  await waitFor(() => fixture.client.subscriptions.length === 1, 'enterprise subscription')

  assert.equal(fixture.application.activeSurface.id, 'enterprise')
  assert.equal(byClass(fixture.rootElement, 'wwc-enterprise-route-root').dataset.enterpriseRoute, 'operations')
  assert.equal(
    byClass(fixture.rootElement, 'wwc-enterprise-navigation-link').getAttribute('aria-current'),
    null,
  )
  const operationLink = descendants(fixture.rootElement).find(node => (
    node.dataset.enterpriseRoute === 'operations'
  ))
  assert.equal(operationLink.getAttribute('aria-current'), 'page')
  assert.equal(
    fixture.client.queries.filter(query => query.query === 'enterprise.audit.list').length,
    2,
  )
  assert.equal(fixture.client.subscriptions[0].options.subscription.stream.kind, 'scope')
  fixture.application.close()
  assert.equal(fixture.client.closed, true)
})

test('permission navigation is fail-closed and invalidation refreshes only its bounded area', async () => {
  const client = enterpriseFacadeFake()
  for (const area of ['organization', 'members', 'projects']) client.allowed[area] = false
  const fixture = mountedFixture('#/enterprise/resources', client)
  await waitFor(() => client.subscriptions.length === 1, 'permission snapshot')

  const resourceLink = descendants(fixture.rootElement).find(node => (
    node.dataset.enterpriseRoute === 'resources'
  ))
  assert.equal(resourceLink.getAttribute('aria-disabled'), 'true')
  assert.equal(resourceLink.tabIndex, -1)
  assert.equal(resourceLink.dispatch('click').defaultPrevented, true)
  assert.equal(
    byClass(fixture.rootElement, 'wwc-enterprise-route-status').textContent,
    'Resources and access is not available to the current identity.',
  )
  assert.equal(visibleText(fixture.rootElement).includes('private permission diagnostics'), false)

  const beforeUsage = client.queries.filter(query => query.query === 'enterprise.usage.list').length
  await client.subscriptions[0].options.onEvent({
    type: 'event.v1',
    subscriptionId: client.subscriptions[0].options.subscriptionId,
    eventId: canonicalId('evt', 1),
    authorizationEpoch: 1,
    occurredAt: '2026-08-27T00:00:00.000Z',
    scope,
    sequence: 1,
    stream: { kind: 'scope' },
    source: { kind: 'control-plane', actor, component: 'enterprise-management' },
    event: {
      type: 'enterprise-usage.invalidated.v1',
      snapshotRevision: 5,
      reloadQueries: ['enterprise.usage.list'],
    },
  })
  assert.equal(
    client.queries.filter(query => query.query === 'enterprise.usage.list').length,
    beforeUsage + 1,
  )
  client.subscriptions[0].options.onError(new ControlPlaneClientError({
    kind: 'network',
    code: 'NETWORK_UNAVAILABLE',
    message: 'private network diagnostics',
    requestId: null,
    retryable: true,
  }))
  assert.equal(
    byClass(fixture.rootElement, 'wwc-enterprise-route-status').textContent,
    'Resources and access is reconnecting to enterprise events.',
  )
  byClass(fixture.rootElement, 'wwc-enterprise-resources-reconnect').dispatch('click')
  assert.equal(client.subscriptions[0].handle.reconnected, true)
  assert.equal(visibleText(fixture.rootElement).includes('private network diagnostics'), false)
  fixture.application.close()
})

test('hash navigation and a new shell preserve enterprise routes without another transport', async () => {
  const firstClient = enterpriseFacadeFake()
  const first = mountedFixture('#/enterprise/operations', firstClient)
  await waitFor(() => firstClient.subscriptions.length === 1, 'first route')

  first.browser.location.hash = '#/enterprise/resources'
  first.browser.dispatch('hashchange')
  await waitFor(
    () => descendants(first.rootElement).some(node => (
      node.className === 'wwc-enterprise-route-root'
      && node.dataset.enterpriseRoute === 'resources'
    )),
    'resource route navigation',
  )
  assert.equal(firstClient.subscriptions[0].handle.closed, true)
  assert.equal(first.application.activeSurface.id, 'enterprise')
  first.application.close()

  const reloaded = mountedFixture('#/enterprise/resources', enterpriseFacadeFake())
  await waitFor(() => reloaded.client.subscriptions.length === 1, 'reloaded deep link')
  assert.equal(
    byClass(reloaded.rootElement, 'wwc-enterprise-route-root').dataset.enterpriseRoute,
    'resources',
  )
  assert.equal(clientSurfaceFromHash('#/enterprise/resources').id, 'enterprise')
  assert.equal(clientSurfaceFromHash('#/enterprise/operations').id, 'enterprise')
  reloaded.application.close()
})

test('restored Actor and reduced Scope are the only enterprise identity source', async () => {
  const reducedScope = {
    kind: 'organization',
    organizationId: 'org_00000000000000000000000002',
  }
  const secondActor = { kind: 'user', id: 'usr_00000000000000000000000002' }
  const reducedClient = enterpriseFacadeFake({
    ...session,
    actor: secondActor,
    authorizedScopes: [reducedScope],
  })
  const reduced = mountedFixture('#/enterprise/resources', reducedClient)
  await waitFor(() => reducedClient.subscriptions.length === 1, 'reduced session context')
  assert.ok(reducedClient.queries.length > 0)
  assert.ok(reducedClient.queries.every(query => (
    JSON.stringify(query.actor) === JSON.stringify(secondActor)
    && JSON.stringify(query.scope) === JSON.stringify(reducedScope)
  )))
  assert.deepEqual(
    reducedClient.subscriptions[0].options.subscription.scope,
    reducedScope,
  )
  reduced.application.close()

  const revokedClient = enterpriseFacadeFake(session, new ControlPlaneClientError({
    kind: 'authentication',
    code: 'AUTHENTICATION_REQUIRED',
    message: 'private revoked-session diagnostics',
    requestId: null,
    retryable: false,
  }))
  const revoked = mountedFixture('#/enterprise/resources', revokedClient)
  await waitFor(
    () => revoked.application.authSession.state.status === 'authentication-required',
    'revoked session rejection',
  )
  assert.equal(revokedClient.queries.length, 0)
  assert.equal(revokedClient.subscriptions.length, 0)
  assert.match(
    byClass(revoked.rootElement, 'wwc-enterprise-context-required').textContent,
    /Sign in/u,
  )
  assert.equal(visibleText(revoked.rootElement).includes('private revoked-session'), false)
  revoked.application.close()
})

test('enterprise shell uses route chunks, one generated facade, and no direct network path', () => {
  const application = readFileSync(resolve(root, 'apps/client/src/application.ts'), 'utf8')
  const enterprise = readFileSync(
    resolve(root, 'apps/client/src/enterprise-application.ts'),
    'utf8',
  )
  assert.match(application, /import\('\.\/enterprise-application\.js'\)/u)
  assert.match(enterprise, /import\('\.\/enterprise-resource-page\.js'\)/u)
  assert.match(enterprise, /import\('\.\/enterprise-operations-page\.js'\)/u)
  assert.equal((enterprise.match(/createEnterpriseManagementViewModel/gu) ?? []).length, 2)
  assert.equal((application.match(/createControlPlaneClient/gu) ?? []).length, 2)
  assert.doesNotMatch(`${application}\n${enterprise}`, /\bfetch\s*\(|new\s+WebSocket/u)
  assert.doesNotMatch(enterprise, /serverUrl|localStorage|sessionStorage|console\./u)
})
