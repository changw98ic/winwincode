import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { resolve } from 'node:path'
import { pathToFileURL } from 'node:url'
import test from 'node:test'

import {
  findByClass,
  TrackedDocument,
  treeNodeCount,
} from './fixtures/ui601-keyed-dom.mjs'

const validationRoot = resolve(import.meta.dirname, '..')
const targetRoot = resolve(process.env.UI601_TARGET_ROOT ?? validationRoot)
const compiler = spawnSync(
  'corepack',
  [
    'pnpm',
    'exec',
    'tsc',
    '-p',
    'apps/client/tsconfig.chat-page-tests.json',
    '--pretty',
    'false',
    '--incremental',
    'false',
  ],
  { cwd: targetRoot, encoding: 'utf8' },
)
assert.equal(
  compiler.status,
  0,
  `Chat page did not compile in ${targetRoot}:\n${compiler.stdout}${compiler.stderr}`,
)

const { mountChatPage } = await import(`${pathToFileURL(resolve(
  targetRoot,
  '.cache/chat-page-tests/chat-page.js',
)).href}?ui601-validation=${String(Date.now())}`)

const firstSessionId = 'psn_00000000000000000000000001'
const secondSessionId = 'psn_00000000000000000000000002'
const firstRoute = {
  providerId: 'provider-one',
  modelId: 'model-one',
  credentialReferenceId: 'crd_PRIVATE_REFERENCE_00000001',
}
const secondRoute = {
  providerId: 'provider-two',
  modelId: 'model-two',
  credentialReferenceId: 'crd_PRIVATE_REFERENCE_00000002',
}
const repositoryScope = {
  kind: 'repository',
  organizationId: 'org_00000000000000000000000001',
  workspaceId: 'wsp_00000000000000000000000001',
  projectId: 'prj_00000000000000000000000001',
  repositoryId: 'rep_00000000000000000000000001',
}

function modelRouteOption(route, isDefault) {
  return {
    route,
    providerDisplayName: route.providerId,
    modelDisplayName: route.modelId,
    catalogSource: repositoryScope,
    catalogVersion: 1,
    providerVersion: 1,
    modelVersion: 1,
    contextWindowTokens: 128_000,
    maxOutputTokens: 16_000,
    toolSupport: 'parallel',
    reasoningEfforts: ['medium', 'high'],
    credentialRotationVersion: 1,
    isDefault,
    status: 'enabled',
    reason: 'ready',
  }
}

function session(id, title, revision = 1) {
  return {
    id,
    projectId: repositoryScope.projectId,
    repositoryId: repositoryScope.repositoryId,
    revision,
    state: 'running',
    title,
    updatedAt: '2026-09-02T00:00:00.000Z',
  }
}

function message(content, sequence = 1) {
  return {
    id: 'msg_00000000000000000000000001',
    productSessionId: firstSessionId,
    role: 'assistant',
    content,
    sequence,
    state: 'streaming',
    createdAt: '2026-09-02T00:00:00.000Z',
    updatedAt: '2026-09-02T00:00:00.000Z',
  }
}

function pageState(update = 0) {
  const first = session(
    firstSessionId,
    update === 100 ? 'Updated Chat' : 'Primary Chat',
    update + 1,
  )
  return {
    status: 'ready',
    realtime: update % 2 === 0 ? 'subscribed' : 'reloading',
    activeProductSessionId: firstSessionId,
    sessions: [first, session(secondSessionId, 'Second Chat')],
    session: first,
    messages: [message(update === 100 ? 'Updated response' : 'Streaming response', update + 1)],
    messagePagination: {
      status: 'idle',
      hasMore: true,
      nextCursor: 'cursor_0000000001',
      error: null,
    },
    defaultModelRoute: firstRoute,
    modelRouteAvailability: {
      kind: 'model_route_availability_page',
      scope: repositoryScope,
      settingsSource: repositoryScope,
      settingsRevision: 1,
      requestPoolSource: {
        kind: 'project',
        organizationId: repositoryScope.organizationId,
        workspaceId: repositoryScope.workspaceId,
        projectId: repositoryScope.projectId,
      },
      requestPoolRevision: 1,
      defaultProviderId: firstRoute.providerId,
      defaultModelId: firstRoute.modelId,
      status: 'enabled',
      reason: 'ready',
      items: [
        modelRouteOption(firstRoute, true),
        modelRouteOption(secondRoute, false),
      ],
    },
    selectedModelRoute: secondRoute,
    modelRouteSelectionIssue: null,
    runtime: null,
    pendingInputs: [],
    pendingApprovals: [],
    interaction: {
      status: update % 2 === 0 ? 'idle' : 'waiting',
      error: null,
    },
    error: null,
  }
}

class PublishingChatModel {
  constructor(initialState) {
    this.state = initialState
  }

  listener = null

  subscribe(listener) {
    this.listener = listener
    listener(this.state)
    return () => { this.listener = null }
  }

  publish(state) {
    this.state = state
    this.listener?.(state)
  }

  async start() {}
  async refresh() {}
  async loadMoreMessages() {}
  async selectSession() {}
  selectModelRoute() {}
  async createSession() {}
  async submitMessage() {}
  async cancelSession() {}
  cancelPending() {}
  reconnect() {}
  close() {}
}

test('100 Chat updates retain keyed identity, user state, bounded nodes, and dispose listeners', () => {
  const document = new TrackedDocument()
  const root = document.createElement('main')
  const model = new PublishingChatModel(pageState())
  const mounted = mountChatPage({
    root,
    model,
    modelRoutes: [secondRoute],
    nextProductSessionId: () => 'psn_00000000000000000000000003',
  })

  const sessionList = findByClass(root, 'wwc-chat-session-list')
  const messageList = findByClass(root, 'wwc-chat-messages')
  const modelSelect = findByClass(root, 'wwc-chat-model')
  const composer = findByClass(root, 'wwc-chat-composer-input')
  assert.notEqual(sessionList, null)
  assert.notEqual(messageList, null)
  assert.notEqual(modelSelect, null)
  assert.notEqual(composer, null)

  const firstSessionRow = sessionList.children[0]
  const firstSessionButton = firstSessionRow.children[0]
  const firstMessageRow = messageList.children[0]
  const selectedModelOption = modelSelect.children[1]
  firstSessionButton.setAttribute('aria-expanded', 'true')
  composer.value = 'unfinished local draft'
  composer.selectionStart = 3
  composer.selectionEnd = 12
  composer.scrollTop = 17
  document.activeElement = composer
  sessionList.scrollTop = 41
  messageList.scrollTop = 79
  modelSelect.selectedIndex = 1

  const createdAtMount = document.elements.length
  const listenersAtMount = document.listenerCount()
  const connectedAtMount = treeNodeCount(root)

  for (let update = 1; update <= 100; update += 1) model.publish(pageState(update))

  assert.ok(sessionList.children[0] === firstSessionRow, 'session row identity changed')
  assert.ok(messageList.children[0] === firstMessageRow, 'message row identity changed')
  assert.ok(
    modelSelect.children[1] === selectedModelOption,
    'selected model option identity changed',
  )
  assert.equal(firstSessionButton.getAttribute('aria-expanded'), 'true')
  assert.equal(firstSessionButton.textContent, 'Updated Chat')
  assert.equal(firstMessageRow.children[0].children[1].textContent, 'Updated response')
  assert.equal(composer.value, 'unfinished local draft')
  assert.equal(composer.selectionStart, 3)
  assert.equal(composer.selectionEnd, 12)
  assert.equal(composer.scrollTop, 17)
  assert.ok(document.activeElement === composer, 'composer focus moved')
  assert.equal(sessionList.scrollTop, 41)
  assert.equal(messageList.scrollTop, 79)
  assert.equal(modelSelect.selectedIndex, 1)
  assert.equal(sessionList.children.length, 2)
  assert.equal(messageList.children.length, 1)
  assert.equal(modelSelect.children.length, 2)
  assert.ok(
    document.elements.length - createdAtMount <= 2,
    `100 updates allocated ${String(document.elements.length - createdAtMount)} extra nodes`,
  )
  assert.equal(treeNodeCount(root), connectedAtMount)
  assert.equal(document.listenerCount(), listenersAtMount)

  mounted.close()
  assert.equal(model.listener, null)
  assert.deepEqual(root.children, [])
  assert.equal(document.listenerCount(), 0)
})
