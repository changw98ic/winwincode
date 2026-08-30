import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { pathToFileURL } from 'node:url'
import { resolve } from 'node:path'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')
const compiler = spawnSync(
  'corepack',
  ['pnpm', 'exec', 'tsc', '-p', 'apps/client/tsconfig.runtime-tests.json', '--pretty', 'false'],
  { cwd: root, encoding: 'utf8' },
)
assert.equal(
  compiler.status,
  0,
  `auth session Client did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const cache = resolve(root, '.cache/control-plane-client-tests')
const run = String(Date.now())
const facade = await import(`${pathToFileURL(resolve(cache, 'control-plane-client.js')).href}?run=${run}`)
const viewModel = await import(`${pathToFileURL(resolve(cache, 'auth-view-model.js')).href}?run=${run}`)
const page = await import(`${pathToFileURL(resolve(cache, 'auth-page.js')).href}?run=${run}`)

const { ControlPlaneClientError, createControlPlaneClient } = facade
const { createAuthSessionViewModel } = viewModel
const { mountAuthSessionPage } = page
const schemaVersion = 'winwincode/v1'
const actor = { kind: 'user', id: 'usr_00000000000000000000000001' }
const authorizedScopes = [
  { kind: 'organization', organizationId: 'org_00000000000000000000000001' },
]

function session(expiresAt = '2026-08-27T08:15:30.123Z') {
  return { schemaVersion, expiresAt, actor, authorizedScopes }
}

function response(status, payload = '') {
  return {
    ok: status >= 200 && status < 300,
    status,
    async text() {
      return typeof payload === 'string' ? payload : JSON.stringify(payload)
    },
  }
}

test('facade restores context, exchanges proof once, and closes the cookie session', async () => {
  const requests = []
  const proof = 'proof-material-only-for-this-call'
  const client = createControlPlaneClient({
    serverUrl: 'https://control.example/root',
    transport: {
      async fetch(input, init) {
        requests.push({ input, init: structuredClone(init) })
        if (init.method === 'DELETE') return response(204)
        return response(init.method === 'GET' ? 200 : 201, session())
      },
    },
  })

  const restored = await client.restore()
  const created = await client.login(proof)
  await client.logout()

  assert.deepEqual(restored, session())
  assert.deepEqual(created, session())
  assert.deepEqual(requests.map(request => [request.input, request.init.method]), [
    ['https://control.example/root/api/v1/auth/session', 'GET'],
    ['https://control.example/root/api/v1/auth/session', 'POST'],
    ['https://control.example/root/api/v1/auth/session', 'DELETE'],
  ])
  for (const request of requests) {
    assert.equal(request.init.credentials, 'include')
    assert.equal(request.init.redirect, 'error')
    assert.equal(request.init.cache, 'no-store')
    assert.equal(request.init.referrerPolicy, 'no-referrer')
  }
  assert.equal(requests[0].init.headers.Authorization, undefined)
  assert.equal(requests[0].init.body, undefined)
  assert.equal(requests[1].init.headers.Authorization, `Bearer ${proof}`)
  assert.equal(requests[2].init.headers.Authorization, undefined)
  assert.deepEqual(JSON.parse(requests[1].init.body), { schemaVersion })
  assert.doesNotMatch(JSON.stringify(client), /proof-material/u)
})

test('facade treats redirect as an error and submits proof exactly once', async () => {
  let requests = 0
  const proof = 'proof-not-forwarded-by-the-client'
  const client = createControlPlaneClient({
    serverUrl: 'https://control.example',
    transport: {
      async fetch(_input, init) {
        requests += 1
        assert.equal(init.redirect, 'error')
        return response(302, '')
      },
    },
  })

  await assert.rejects(
    client.login(proof),
    error => error instanceof ControlPlaneClientError
      && error.code === 'INVALID_AUTH_SESSION_RESPONSE'
      && !error.message.includes(proof),
  )
  assert.equal(requests, 1)
})

test('facade rejects expired proof and malformed expiresAt without echoing proof', async () => {
  const proof = 'proof-must-not-appear-in-errors'
  let mode = 'denied'
  const client = createControlPlaneClient({
    serverUrl: 'https://control.example',
    transport: {
      async fetch() {
        if (mode === 'denied') return response(401, {
          schemaVersion,
          requestId: 'req_00000000000000000000000001',
          error: {
            code: 'AUTHENTICATION_REQUIRED',
            message: 'authentication failed',
            retryable: false,
            details: { reason: 'AUTHENTICATION_REQUIRED' },
          },
        })
        return response(201, session('Thu, 27 Aug 2026 08:15:30 GMT'))
      },
    },
  })

  await assert.rejects(
    client.login(proof),
    error => error instanceof ControlPlaneClientError
      && error.kind === 'authentication'
      && !JSON.stringify(error).includes(proof)
      && !error.message.includes(proof),
  )
  mode = 'malformed'
  await assert.rejects(
    client.login(proof),
    error => error instanceof ControlPlaneClientError
      && error.code === 'INVALID_AUTH_SESSION_RESPONSE'
      && !error.message.includes(proof),
  )
})

test('facade rejects unbounded, duplicate, or non-canonical identity context', async () => {
  const invalidResponses = [
    { ...session(), actor: { kind: 'user', id: 'user-1' } },
    { ...session(), authorizedScopes: [] },
    { ...session(), authorizedScopes: [authorizedScopes[0], authorizedScopes[0]] },
    { ...session(), privateRole: 'owner' },
  ]
  let index = 0
  const client = createControlPlaneClient({
    serverUrl: 'https://control.example',
    transport: {
      async fetch() {
        const payload = invalidResponses[index]
        index += 1
        return response(200, payload)
      },
    },
  })
  for (const invalid of invalidResponses) {
    await assert.rejects(
      client.restore(),
      error => error instanceof ControlPlaneClientError
        && error.code === 'INVALID_AUTH_SESSION_RESPONSE'
        && !JSON.stringify(error).includes(JSON.stringify(invalid)),
    )
  }
})

class FakeElement {
  constructor(tagName, document) {
    this.tagName = tagName.toUpperCase()
    this.ownerDocument = document
  }

  children = []
  listeners = new Map()
  attributes = new Map()
  className = ''
  textContent = ''
  value = ''
  hidden = false
  disabled = false
  id = ''
  type = ''
  autocomplete = ''
  spellcheck = true
  htmlFor = ''

  append(...children) {
    this.children.push(...children)
  }

  replaceChildren(...children) {
    this.children = [...children]
  }

  setAttribute(name, value) {
    this.attributes.set(name, value)
  }

  addEventListener(type, listener) {
    const listeners = this.listeners.get(type) ?? new Set()
    listeners.add(listener)
    this.listeners.set(type, listeners)
  }

  removeEventListener(type, listener) {
    this.listeners.get(type)?.delete(listener)
  }

  emit(type, event = {}) {
    for (const listener of this.listeners.get(type) ?? []) listener(event)
  }
}

class FakeDocument {
  createElement(tagName) {
    return new FakeElement(tagName, this)
  }
}

function descendants(node) {
  return [node, ...node.children.flatMap(child => descendants(child))]
}

test('login page clears proof before facade submission and view-model never stores it', async () => {
  const document = new FakeDocument()
  const rootElement = new FakeElement('div', document)
  const proof = 'proof-never-rendered-or-stored'
  let submitted = null
  let release
  const client = {
    async restore() { return session() },
    async login(value) {
      submitted = value
      await new Promise(resolvePromise => { release = resolvePromise })
      return session('2026-08-27T08:15:30Z')
    },
    async logout() {},
  }
  const model = createAuthSessionViewModel(client)
  const mounted = mountAuthSessionPage({ root: rootElement, model })
  const nodes = descendants(rootElement)
  const input = nodes.find(node => node.className === 'wwc-auth-session-proof')
  const form = nodes.find(node => node.className === 'wwc-auth-session-form')
  input.value = proof

  form.emit('submit', { preventDefault() {} })

  assert.equal(input.value, '')
  assert.equal(submitted, proof)
  assert.doesNotMatch(JSON.stringify(model.state), /proof-never/u)
  assert.doesNotMatch(
    descendants(rootElement).map(node => node.textContent).join(' '),
    /proof-never/u,
  )
  release()
  await new Promise(resolvePromise => setImmediate(resolvePromise))
  assert.equal(model.state.status, 'signed-in')
  assert.doesNotMatch(JSON.stringify(model.state), /proof-never/u)

  mounted.close()
  model.close()
  assert.deepEqual(rootElement.children, [])
})
