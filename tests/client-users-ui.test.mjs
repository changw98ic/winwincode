import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { pathToFileURL } from 'node:url'
import { resolve } from 'node:path'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')
const compiler = spawnSync(
  'corepack',
  [
    'pnpm',
    'exec',
    'tsc',
    '-p',
    'apps/client/tsconfig.client-users-tests.json',
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
  `Users area did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const cache = resolve(root, '.cache/client-users-tests')
// Plain module paths keep one ControlPlaneClientError class identity across
// the facade, the view-models, and these assertions.
async function cachedModule(name) {
  return import(pathToFileURL(resolve(cache, name)).href)
}
const facade = await cachedModule('control-plane-client.js')
const viewModelModule = await cachedModule('user-management-view-model.js')
const pageModule = await cachedModule('users-page.js')

const { ControlPlaneClientError } = facade
const {
  createUserManagementViewModel,
  userManagementPortFromFacade,
} = viewModelModule
const { mountUsersPage } = pageModule

const ownerId = 'usr_00000000000000000000000001'

function user(overrides = {}) {
  return {
    userId: 'usr_00000000000000000000000002',
    username: 'ada',
    role: 'member',
    state: 'active',
    createdAt: '2026-09-01T00:00:00.000Z',
    revision: 1,
    ...overrides,
  }
}

function usersError(code, kind = 'server') {
  return new ControlPlaneClientError({
    kind,
    code,
    message: 'user management rejected',
    requestId: null,
    retryable: false,
  })
}

/**
 * One deterministic users port: every write records its payload and waits for
 * a settle; the list read serves the current mutable snapshot and counts its
 * own reads.
 */
function portFake(initialUsers) {
  const users = initialUsers
  const calls = []
  const listCalls = []
  const pending = []
  function track(method, payload) {
    calls.push({ method, ...payload })
    return new Promise((resolvePromise, rejectPromise) => {
      pending.push({ method, resolve: resolvePromise, reject: rejectPromise })
    })
  }
  return {
    users,
    calls,
    listCalls,
    pending,
    listUsers() {
      listCalls.push([...users])
      return Promise.resolve([...users])
    },
    create(input) { return track('create', { ...input }) },
    setState(input) { return track('setState', { ...input }) },
    resetPassword(input) { return track('resetPassword', { ...input }) },
  }
}

async function usersFixture({
  users = [user()],
  port = null,
  selfUserId = ownerId,
  writeText = null,
} = {}) {
  const resolvedPort = port ?? portFake(users)
  const document = new PageDocument()
  const rootElement = new PageElement(document, 'div')
  const model = createUserManagementViewModel({
    port: resolvedPort,
    ...(selfUserId === null ? {} : { selfUserId }),
  })
  const page = mountUsersPage({
    root: rootElement,
    model,
    ...(selfUserId === null ? {} : { selfUserId }),
    ...(writeText === null ? {} : { writeText }),
  })
  page.setVisible(true)
  await model.refresh()
  return { rootElement, model, page, port: resolvedPort }
}

class PageElement {
  constructor(ownerDocument, tagName) {
    this.ownerDocument = ownerDocument
    this.tagName = tagName.toUpperCase()
    this.attributes = new Map()
    this.children = []
    this.listeners = new Map()
    this.dataset = {}
    this.className = ''
    this.disabled = false
    this.hidden = false
    this.tabIndex = 0
    this.href = ''
    this.id = ''
    this.htmlFor = ''
    this.name = ''
    this.required = false
    this.spellcheck = true
    this.autocomplete = ''
    this.type = ''
    this.value = ''
    this.maxLength = -1
    this.#textContent = ''
    this.parentNode = null
    this.checkValidity = () => true
  }
  #textContent = ''
  get childNodes() { return this.children }
  get textContent() { return this.#textContent }
  set textContent(value) {
    this.#textContent = String(value)
    this.children = []
  }
  append(...children) {
    for (const child of children) child.parentNode = this
    this.children.push(...children)
  }
  replaceChildren(...children) {
    for (const child of this.children) child.parentNode = null
    for (const child of children) child.parentNode = this
    this.children = [...children]
  }
  insertBefore(node, current) {
    this.children = this.children.filter(child => child !== node)
    const index = current === null ? -1 : this.children.indexOf(current)
    if (index < 0) this.children.push(node)
    else this.children.splice(index, 0, node)
    node.parentNode = this
  }
  setAttribute(name, value) { this.attributes.set(name, String(value)) }
  getAttribute(name) { return this.attributes.get(name) ?? null }
  removeAttribute(name) { this.attributes.delete(name) }
  remove() {
    const parent = this.parentNode
    if (parent !== null) parent.children = parent.children.filter(child => child !== this)
    this.parentNode = null
  }
  addEventListener(name, listener) {
    const current = this.listeners.get(name) ?? []
    current.push(listener)
    this.listeners.set(name, current)
  }
  removeEventListener(name, listener) {
    this.listeners.set(
      name,
      (this.listeners.get(name) ?? []).filter(candidate => candidate !== listener),
    )
  }
  dispatchEvent(event) {
    for (const listener of this.listeners.get(event.type) ?? []) listener(event)
    return !event.defaultPrevented
  }
  emit(type, event = {}) { this.dispatchEvent({ type, ...event }) }
}

class PageDocument {
  createElement(tagName) { return new PageElement(this, tagName) }
}

function pageDescendants(node) {
  return [node, ...node.children.flatMap(child => pageDescendants(child))]
}

function hasClass(node, className) {
  return node.className.split(/\s+/u).includes(className)
}

function findOne(rootElement, className, scope = rootElement) {
  const node = pageDescendants(scope).find(candidate => hasClass(candidate, className))
  assert.notEqual(node, undefined, `${className} is mounted`)
  return node
}

function findOptional(rootElement, className) {
  return pageDescendants(rootElement).find(candidate => hasClass(candidate, className)) ?? null
}

function rowAt(rootElement, index) {
  const rows = pageDescendants(rootElement)
    .filter(candidate => hasClass(candidate, 'wwc-users-row'))
  assert.ok(rows.length > index, `row ${index} is mounted`)
  return rows[index]
}

function allRows(rootElement) {
  return pageDescendants(rootElement)
    .filter(candidate => hasClass(candidate, 'wwc-users-row'))
}

function submitForm(form) {
  form.emit('submit', { preventDefault: () => {} })
}

function waitFor(predicate, label) {
  return (async () => {
    const deadline = Date.now() + 5_000
    while (Date.now() < deadline) {
      if (predicate()) return
      await new Promise(resolvePromise => setTimeout(resolvePromise, 10))
    }
    assert.fail(`timed out waiting for ${label}`)
  })()
}

test('account rows render the username, role, state, and creation date', async () => {
  const { rootElement, model, page } = await usersFixture({
    users: [
      user(),
      user({
        userId: 'usr_00000000000000000000000001',
        username: 'owner',
        role: 'owner',
      }),
      user({
        userId: 'usr_00000000000000000000000003',
        username: 'grace',
        state: 'disabled',
      }),
    ],
  })
  const firstRow = rowAt(rootElement, 0)
  assert.equal(findOne(firstRow, 'wwc-users-row-username').textContent, 'ada')
  assert.equal(findOne(firstRow, 'wwc-users-row-role').textContent, 'Member')
  assert.equal(findOne(firstRow, 'wwc-users-row-state').textContent, 'Active')
  assert.equal(findOne(firstRow, 'wwc-users-row-created').textContent, 'Created 2026-09-01')
  assert.equal(findOne(firstRow, 'wwc-users-row-state').dataset.tone, 'success')

  assert.equal(findOne(rowAt(rootElement, 1), 'wwc-users-row-role').textContent, 'Owner')
  const disabledRow = rowAt(rootElement, 2)
  assert.equal(findOne(disabledRow, 'wwc-users-row-state').textContent, 'Disabled')
  assert.equal(findOne(disabledRow, 'wwc-users-row-state').dataset.tone, 'danger')

  // Each direction offers exactly the state change the row supports.
  assert.equal(
    findOne(firstRow, 'wwc-users-row-state-button').textContent,
    'Disable user',
  )
  assert.equal(
    findOne(disabledRow, 'wwc-users-row-state-button').textContent,
    'Enable user',
  )
  page.close()
  model.close()
})

test('creating a user shows the one-time password exactly once and clears the draft', async () => {
  const port = portFake([user()])
  const { rootElement, model, page } = await usersFixture({ port })
  const form = findOne(rootElement, 'wwc-users-create-form')
  const usernameInput = findOne(form, 'wwc-users-username-input')
  const roleSelect = findOne(form, 'wwc-users-role-select')
  usernameInput.value = '  grace  '
  roleSelect.value = 'member'

  submitForm(form)
  assert.deepEqual(port.calls, [{ method: 'create', username: 'grace', role: 'member' }])
  assert.equal(findOne(form, 'wwc-users-create-submit').disabled, true)
  submitForm(form)
  assert.equal(port.calls.length, 1, 'a submission in progress is never repeated')

  port.pending[0].resolve({
    user: user({ username: 'grace', revision: 1 }),
    temporaryPassword: 'one-time-secret-1',
  })
  await waitFor(
    () => findOptional(rootElement, 'wwc-users-one-time')?.hidden === false,
    'the one-time region appears',
  )
  const region = findOne(rootElement, 'wwc-users-one-time')
  assert.equal(
    findOne(region, 'wwc-users-one-time-title').textContent,
    'Account created. One-time password for grace',
  )
  assert.equal(findOne(region, 'wwc-users-one-time-secret').textContent, 'one-time-secret-1')
  assert.match(
    findOne(region, 'wwc-users-one-time-hint').textContent,
    /shown only once/,
    'the copy hint tells the reader the secret is one-time',
  )
  assert.equal(findOne(rootElement, 'wwc-users-username-input').value, '')
  assert.equal(findOne(rootElement, 'wwc-users-role-select').value, 'member')
  assert.equal(findOne(form, 'wwc-users-create-submit').disabled, false)

  // The dismissed secret is final: it never comes back through a re-render.
  findOne(region, 'wwc-users-one-time-done').emit('click')
  assert.equal(findOne(rootElement, 'wwc-users-one-time').hidden, true)
  assert.equal(findOne(rootElement, 'wwc-users-one-time-secret').textContent, '')
  await model.refresh()
  assert.equal(findOne(rootElement, 'wwc-users-one-time').hidden, true)
  page.close()
  model.close()
})

test('the Owner reset surfaces its fresh secret through the same one-time region', async () => {
  const port = portFake([user()])
  const { rootElement, model, page } = await usersFixture({ port })
  const row = rowAt(rootElement, 0)
  findOne(row, 'wwc-users-row-reset').emit('click')

  assert.deepEqual(port.calls, [{
    method: 'resetPassword',
    userId: 'usr_00000000000000000000000002',
    expectedRevision: 1,
  }])
  assert.equal(findOne(row, 'wwc-users-row-reset').textContent, 'Resetting…')
  assert.equal(findOne(row, 'wwc-users-row-reset').disabled, true)

  port.pending[0].resolve({
    user: user({ revision: 2 }),
    temporaryPassword: 'one-time-secret-2',
  })
  await waitFor(
    () => findOptional(rootElement, 'wwc-users-one-time')?.hidden === false,
    'the reset secret region appears',
  )
  const region = findOne(rootElement, 'wwc-users-one-time')
  assert.equal(
    findOne(region, 'wwc-users-one-time-title').textContent,
    'Password reset. One-time password for ada',
  )
  assert.equal(findOne(region, 'wwc-users-one-time-secret').textContent, 'one-time-secret-2')

  // A second reset replaces the displayed secret: the old one is never shown
  // twice, the new one is.
  findOne(row, 'wwc-users-row-reset').emit('click')
  port.pending[1].resolve({
    user: user({ revision: 3 }),
    temporaryPassword: 'one-time-secret-3',
  })
  await waitFor(
    () => findOne(rootElement, 'wwc-users-one-time-secret').textContent === 'one-time-secret-3',
    'the fresh reset secret replaces the old one',
  )
  page.close()
  model.close()
  assert.equal(
    model.state.oneTime,
    null,
    'the one-time secret never survives the page',
  )
})

test('the copy hint works through the injected clipboard and reports failure', async () => {
  const clipboardCalls = []
  const port = portFake([user()])
  const { rootElement, model, page } = await usersFixture({
    port,
    writeText: async text => {
      clipboardCalls.push(text)
      if (clipboardCalls.length === 1) return
      throw new Error('clipboard blocked')
    },
  })
  const row = rowAt(rootElement, 0)
  findOne(row, 'wwc-users-row-reset').emit('click')
  port.pending[0].resolve({
    user: user({ revision: 2 }),
    temporaryPassword: 'one-time-secret-2',
  })
  await waitFor(
    () => findOptional(rootElement, 'wwc-users-one-time')?.hidden === false,
    'the reset secret region appears',
  )
  const region = findOne(rootElement, 'wwc-users-one-time')
  findOne(region, 'wwc-users-one-time-copy').emit('click')
  await waitFor(
    () => findOptional(rootElement, 'wwc-users-one-time-copied')?.hidden === false,
    'the copy confirmation appears',
  )
  assert.equal(
    findOne(region, 'wwc-users-one-time-copied').textContent,
    'Copied to the clipboard.',
  )
  assert.deepEqual(clipboardCalls, ['one-time-secret-2'])

  findOne(region, 'wwc-users-one-time-copy').emit('click')
  await waitFor(
    () => findOne(region, 'wwc-users-one-time-copied').textContent.includes('Copy failed'),
    'the copy failure hint appears',
  )
  page.close()
  model.close()
})

test('disable and enable always pass the explicit confirmation first', async () => {
  const port = portFake([
    user(),
    user({
      userId: 'usr_00000000000000000000000003',
      username: 'grace',
      state: 'disabled',
      revision: 4,
    }),
  ])
  const { rootElement, model, page } = await usersFixture({ port })
  const activeRow = rowAt(rootElement, 0)
  const stateButton = findOne(activeRow, 'wwc-users-row-state-button')

  stateButton.emit('click')
  assert.equal(port.calls.length, 0, 'the dangerous state change waits for the accept')
  const confirm = findOne(activeRow, 'wwc-users-row-confirm')
  assert.equal(confirm.hidden, false)
  assert.equal(
    findOne(confirm, 'wwc-users-row-confirm-text').textContent,
    'Disabling signs this user out everywhere and blocks further sign-in.',
  )
  assert.equal(findOne(confirm, 'wwc-users-row-confirm-accept').textContent, 'Disable user')

  findOne(confirm, 'wwc-users-row-confirm-accept').emit('click')
  assert.deepEqual(port.calls, [{
    method: 'setState',
    userId: 'usr_00000000000000000000000002',
    expectedRevision: 1,
    state: 'disabled',
  }])
  assert.equal(stateButton.textContent, 'Disabling…')
  assert.equal(stateButton.disabled, true)

  // The list read stays the single authority: the fresh snapshot is in place
  // before the write settles, mirroring the Server handoff.
  port.users[0] = user({ state: 'disabled', revision: 2 })
  port.pending[0].resolve(user({ state: 'disabled', revision: 2 }))
  await waitFor(
    () => findOne(activeRow, 'wwc-users-row-state').textContent === 'Disabled',
    'the refreshed account state reaches the row',
  )
  assert.equal(findOne(activeRow, 'wwc-users-row-confirm').hidden, true)

  // The disabled row now offers the enable direction, also confirmed.
  const enabledRow = rowAt(rootElement, 1)
  const enableButton = findOne(enabledRow, 'wwc-users-row-state-button')
  enableButton.emit('click')
  const enableConfirm = findOne(enabledRow, 'wwc-users-row-confirm')
  assert.equal(enableConfirm.hidden, false)
  assert.equal(
    findOne(enableConfirm, 'wwc-users-row-confirm-text').textContent,
    'Enabling restores this account\'s sign-in immediately.',
  )
  findOne(enableConfirm, 'wwc-users-row-confirm-accept').emit('click')
  assert.deepEqual(port.calls.at(-1), {
    method: 'setState',
    userId: 'usr_00000000000000000000000003',
    expectedRevision: 4,
    state: 'active',
  })
  port.users[1] = user({
    userId: 'usr_00000000000000000000000003',
    username: 'grace',
    state: 'active',
    revision: 5,
  })
  port.pending[1].resolve(port.users[1])
  await waitFor(
    () => findOne(enabledRow, 'wwc-users-row-state').textContent === 'Active',
    'the enable lands',
  )
  page.close()
  model.close()
})

test('Keep drops the armed state change without submitting', async () => {
  const port = portFake([user()])
  const { rootElement, model, page } = await usersFixture({ port })
  const row = rowAt(rootElement, 0)
  findOne(row, 'wwc-users-row-state-button').emit('click')
  assert.equal(findOne(row, 'wwc-users-row-confirm').hidden, false)

  findOne(row, 'wwc-users-row-confirm-keep').emit('click')
  assert.equal(findOne(row, 'wwc-users-row-confirm').hidden, true)
  assert.equal(port.calls.length, 0)
  assert.equal(model.rowInteraction('usr_00000000000000000000000002').kind, 'rest')

  // A fresh draft can be armed again after the drop.
  findOne(row, 'wwc-users-row-state-button').emit('click')
  assert.equal(findOne(row, 'wwc-users-row-confirm').hidden, false)
  page.close()
  model.close()
})

test('a failed state change keeps its armed draft and the same accept retries', async () => {
  const port = portFake([user()])
  const { rootElement, model, page } = await usersFixture({ port })
  const row = rowAt(rootElement, 0)
  findOne(row, 'wwc-users-row-state-button').emit('click')
  findOne(row, 'wwc-users-row-confirm-accept').emit('click')
  port.pending[0].reject(usersError('REVISION_CONFLICT'))

  await waitFor(
    () => model.rowInteraction('usr_00000000000000000000000002').kind === 'failed',
    'the rejection reaches the interaction',
  )
  assert.equal(
    findOne(row, 'wwc-users-row-error').textContent,
    'The account changed while you worked. Retry on the current state.',
  )
  assert.equal(findOne(row, 'wwc-users-row-error').hidden, false)
  assert.equal(
    findOne(row, 'wwc-users-row-confirm').hidden,
    false,
    'the armed confirmation survives the failure as the retry draft',
  )

  findOne(row, 'wwc-users-row-confirm-accept').emit('click')
  assert.equal(port.calls.length, 2, 'the same explicit accept retries the request')
  port.pending[1].resolve(user({ state: 'disabled', revision: 7 }))
  await waitFor(
    () => model.rowInteraction('usr_00000000000000000000000002').kind === 'rest',
    'the retry settles',
  )
  assert.equal(findOne(row, 'wwc-users-row-error').hidden, true)
  page.close()
  model.close()
})

test('every taxonomy failure carries its own honest row copy', async () => {
  const failures = [
    ['WRONG_STATE', 'server', 'The account already changed state. Retry to confirm.'],
    ['REVISION_CONFLICT', 'server', 'The account changed while you worked. Retry on the current state.'],
    ['RESOURCE_NOT_FOUND', 'server', 'This account no longer exists.'],
    ['PERMISSION_DENIED', 'authorization', 'Only the Owner can manage users.'],
    ['AUTHENTICATION_REQUIRED', 'authentication', 'Sign in again to continue managing users.'],
    ['SERVICE_UNAVAILABLE', 'server', 'The request did not go through. Check the connection and try again.'],
  ]
  for (const [code, kind, copy] of failures) {
    const port = portFake([user()])
    const { rootElement, model, page } = await usersFixture({ port })
    const row = rowAt(rootElement, 0)
    findOne(row, 'wwc-users-row-reset').emit('click')
    port.pending[0].reject(usersError(code, kind))
    await waitFor(
      () => model.resetInteraction('usr_00000000000000000000000002').kind === 'failed',
      `the ${code} rejection reaches the interaction`,
    )
    assert.equal(findOne(row, 'wwc-users-row-error').hidden, false)
    assert.equal(findOne(row, 'wwc-users-row-error').textContent, copy)
    page.close()
    model.close()
  }
})

test('a failed creation shows the classified copy and editing clears it', async () => {
  const port = portFake([user()])
  const { rootElement, model, page } = await usersFixture({ port })
  const form = findOne(rootElement, 'wwc-users-create-form')
  findOne(form, 'wwc-users-username-input').value = 'ada'
  submitForm(form)
  port.pending[0].reject(usersError('WRONG_STATE'))

  await waitFor(
    () => findOptional(rootElement, 'wwc-users-create-error')?.hidden === false,
    'the create failure appears',
  )
  assert.equal(
    findOne(form, 'wwc-users-create-error').textContent,
    'That username already belongs to another account.',
  )

  findOne(form, 'wwc-users-username-input').emit('input')
  assert.equal(findOne(form, 'wwc-users-create-error').hidden, true)
  page.close()
  model.close()
})

test('the self-service form proves the current password and reports the wrong proof', async () => {
  const port = portFake([
    user(),
    user({
      userId: ownerId,
      username: 'owner',
      role: 'owner',
    }),
  ])
  const { rootElement, model, page } = await usersFixture({
    port,
    selfUserId: ownerId,
  })
  const form = findOne(rootElement, 'wwc-users-self-form')
  const currentInput = findOne(form, 'wwc-users-current-input')
  const newInput = findOne(form, 'wwc-users-new-input')
  currentInput.value = 'current-secret-1'
  newInput.value = 'rotated-password-9'

  submitForm(form)
  assert.deepEqual(port.calls.at(-1), {
    method: 'resetPassword',
    userId: ownerId,
    expectedRevision: 1,
    currentPassword: 'current-secret-1',
    newPassword: 'rotated-password-9',
  })
  // Secret-safe submission: both passwords left the DOM before the await.
  assert.equal(currentInput.value, '')
  assert.equal(newInput.value, '')

  port.pending[0].reject(usersError('AUTHENTICATION_REQUIRED', 'authentication'))
  await waitFor(
    () => model.selfPassword.kind === 'failed',
    'the wrong-proof rejection reaches the form',
  )
  assert.equal(
    findOne(form, 'wwc-users-self-error').textContent,
    'The current password is wrong. Check it and try again.',
  )

  currentInput.value = 'current-secret-1'
  newInput.value = 'rotated-password-9'
  submitForm(form)
  port.pending[1].resolve({
    user: user({ userId: ownerId, username: 'owner', role: 'owner', revision: 2 }),
  })
  await waitFor(
    () => model.selfPassword.kind === 'succeeded',
    'the rotation lands',
  )
  assert.equal(
    findOne(form, 'wwc-users-self-status').textContent,
    'Your password is updated.',
  )
  page.close()
  model.close()
})

test('the self-service form is hidden without a signed-in account id', async () => {
  const port = portFake([user()])
  const { rootElement, model, page } = await usersFixture({
    port,
    selfUserId: null,
  })
  assert.equal(findOne(rootElement, 'wwc-users-self-form').hidden, true)
  page.close()
  model.close()
})

test('a failed list read keeps the rows and the armed draft', async () => {
  const port = portFake([user()])
  const { rootElement, model, page } = await usersFixture({ port })
  const row = rowAt(rootElement, 0)
  findOne(row, 'wwc-users-row-state-button').emit('click')
  assert.equal(findOne(row, 'wwc-users-row-confirm').hidden, false)

  port.listUsers = async () => {
    throw usersError('SERVICE_UNAVAILABLE')
  }
  await model.refresh()
  assert.equal(allRows(rootElement).length, 1, 'the failed read never erases the rows')
  assert.equal(
    model.rowInteraction('usr_00000000000000000000000002').kind,
    'confirming',
    'the armed draft survives a read failure',
  )
  page.close()
  model.close()
})

test('a snapshot that moves past the armed draft drops it without submitting', async () => {
  const port = portFake([user()])
  const { rootElement, model, page } = await usersFixture({ port })
  const row = rowAt(rootElement, 0)
  findOne(row, 'wwc-users-row-state-button').emit('click')
  assert.equal(findOne(row, 'wwc-users-row-confirm').hidden, false)

  // The account leaves the list while the draft is armed.
  port.users.length = 0
  await model.refresh()
  assert.equal(allRows(rootElement).length, 0)
  assert.equal(
    model.rowInteraction('usr_00000000000000000000000002').kind,
    'rest',
    'the stale draft is dropped',
  )
  assert.equal(port.calls.length, 0, 'a stale draft never reaches the facade')
  page.close()
  model.close()
})

test('the re-render keeps the row node identity in place', async () => {
  const port = portFake([user()])
  const { rootElement, model, page } = await usersFixture({ port })
  const row = rowAt(rootElement, 0)
  findOne(row, 'wwc-users-row-state-button').emit('click')
  await waitFor(
    () => findOne(row, 'wwc-users-row-confirm').hidden === false,
    'the row re-renders the armed draft',
  )
  assert.equal(rowAt(rootElement, 0), row, 'the interaction never recreates the row')
  page.close()
  model.close()
})

test('a port-less composition reports the honest unavailable failure', async () => {
  const document = new PageDocument()
  const rootElement = new PageElement(document, 'div')
  const model = createUserManagementViewModel({ port: null })
  const page = mountUsersPage({ root: rootElement, model })
  page.setVisible(true)
  await model.refresh()
  const form = findOne(rootElement, 'wwc-users-create-form')
  findOne(form, 'wwc-users-username-input').value = 'ada'
  submitForm(form)
  await waitFor(
    () => model.state.failure === 'unavailable',
    'the port-less create reports unavailable',
  )
  assert.equal(
    findOne(form, 'wwc-users-create-error').textContent,
    'The request did not go through. Check the connection and try again.',
  )
  page.close()
  model.close()
})

test('the facade adapter rejects incomplete facades and passes through writes', async () => {
  const calls = []
  const completeFacade = {
    listUsers: () => { calls.push('listUsers'); return Promise.resolve([]) },
    createUser: input => { calls.push(['createUser', input.username]); return Promise.resolve({}) },
    setUserState: input => { calls.push(['setUserState', input.state]); return Promise.resolve({}) },
    resetUserPassword: input => {
      calls.push(['resetUserPassword', input.userId])
      return Promise.resolve({})
    },
  }
  const port = userManagementPortFromFacade(completeFacade)
  assert.notEqual(port, null)
  await port.listUsers()
  await port.create({ username: 'ada', role: 'member' })
  await port.setState({ userId: 'usr_1', expectedRevision: 1, state: 'disabled' })
  await port.resetPassword({ userId: 'usr_1', expectedRevision: 2 })
  assert.deepEqual(calls, [
    'listUsers',
    ['createUser', 'ada'],
    ['setUserState', 'disabled'],
    ['resetUserPassword', 'usr_1'],
  ])

  assert.equal(
    userManagementPortFromFacade({
      listUsers: completeFacade.listUsers,
      createUser: completeFacade.createUser,
      setUserState: completeFacade.setUserState,
    }),
    null,
    'an incomplete facade composes no port',
  )
})
