// SPDX-License-Identifier: Apache-2.0

import { mountWinWinCodeClient } from '/module/application.js'

// Deterministic UI-504 Home dashboard.  The injected Control Plane facade serves
// two authorized repository Scopes whose Deliveries and decisions differ, so the
// suite can prove a Scope switch re-reads and re-renders the dashboard in
// isolation.  SECRET_MARKER is planted inside one served Delivery requirement and
// must never reach the DOM, the URL, or browser storage.
const schemaVersion = 'winwincode/v1'
const actor = { kind: 'user', id: 'usr_00000000000000000000000001' }
const identity = {
  organizationId: 'org_00000000000000000000000001',
  workspaceId: 'wsp_00000000000000000000000001',
  projectId: 'prj_00000000000000000000000001',
}
const repositoryOne = {
  kind: 'repository',
  ...identity,
  repositoryId: 'rep_00000000000000000000000001',
}
const repositoryTwo = {
  kind: 'repository',
  ...identity,
  repositoryId: 'rep_00000000000000000000000002',
}
const repositoryThree = {
  kind: 'repository',
  ...identity,
  repositoryId: 'rep_00000000000000000000000003',
}
const SECRET_MARKER = 'vault-locator-secret-marker'
const queries = []

function canonicalId(prefix, value) {
  return `${prefix}_${String(value).padStart(26, '0')}`
}

function response(request, result) {
  return {
    schemaVersion,
    requestId: request.requestId,
    query: request.query,
    result,
    page: { hasMore: false, nextCursor: null },
  }
}

function selection(repository) {
  return {
    organizationId: repository.organizationId,
    workspaceId: repository.workspaceId,
    projectId: repository.projectId,
    repositoryId: repository.repositoryId,
  }
}

function deliveryFor(repository, overrides = {}) {
  const index = Number(repository.repositoryId.slice(-2))
  return {
    activeStageRunId: canonicalId('str', index),
    deliveryId: canonicalId('dlv', index),
    openAttentionCount: 1,
    ownership: {
      organizationId: repository.organizationId,
      workspaceId: repository.workspaceId,
      projectId: repository.projectId,
      repositoryId: repository.repositoryId,
    },
    revision: 4,
    schemaVersion,
    status: 'executing',
    taskCounts: { active: 1, blocked: 0, completed: 1, failed: 2, pending: 0, total: 4, verifying: 0 },
    title: `Delivery of repository ${String(index)}`,
    updatedAt: '2026-09-03T08:00:00.000Z',
    ...overrides,
  }
}

function deliveredFor(repository) {
  const index = Number(repository.repositoryId.slice(-2))
  return deliveryFor(repository, {
    activeStageRunId: null,
    deliveryId: canonicalId('dlv', index + 10),
    openAttentionCount: 0,
    status: 'delivered',
    taskCounts: { active: 0, blocked: 0, completed: 4, failed: 0, pending: 0, total: 4, verifying: 0 },
    title: `Delivered of repository ${String(index)}`,
  })
}

function deliveryDetail(delivery) {
  return {
    deliveryId: delivery.deliveryId,
    deliveryRevision: delivery.revision,
    ownership: delivery.ownership,
    attention: delivery.openAttentionCount > 0
      ? [{
          assignedTo: null,
          blocking: true,
          createdAt: '2026-09-03T08:45:00.000Z',
          deliverySpecId: 'spec-1',
          id: canonicalId('att', 1),
          options: [],
          resolutionSummary: null,
          resolvedAt: null,
          resolvedBy: null,
          stageRunId: delivery.activeStageRunId,
          status: 'open',
          title: 'Review the proposed delivery scope',
          type: 'scope_change',
        }]
      : [],
    currentCandidate: null,
    requirements: {
      repository: { kind: 'local-git', locator: SECRET_MARKER },
    },
    internalToolPayload: SECRET_MARKER,
  }
}

function sessionFor(repository) {
  const index = Number(repository.repositoryId.slice(-2))
  return {
    id: canonicalId('psn', index),
    projectId: repository.projectId,
    repositoryId: repository.repositoryId,
    revision: 2,
    state: 'waiting_for_approval',
    title: `Chat of repository ${String(index)}`,
    updatedAt: '2026-09-03T08:30:00.000Z',
  }
}

function approvalFor(repository) {
  const index = Number(repository.repositoryId.slice(-2))
  const productSessionId = canonicalId('psn', index)
  return {
    id: canonicalId('apr', index),
    revision: 5,
    state: 'pending',
    requestedAt: '2026-09-03T08:40:00.000Z',
    expiresAt: '2099-09-03T10:00:00.000Z',
    subject: 'Allow the projected repository action',
    binding: {
      productSessionId,
      executionJobId: canonicalId('job', index),
      workerSessionId: canonicalId('wss', index),
      sessionIdentity: {
        productSessionId,
        workerSessionId: canonicalId('wss', index),
        codexThreadId: canonicalId('thr', index),
        stageRunId: canonicalId('str', index),
      },
    },
  }
}

const workspace = new Map([
  [repositoryOne.repositoryId, {
    repository: repositoryOne,
    deliveries: [deliveryFor(repositoryOne), deliveredFor(repositoryOne)],
    sessions: [sessionFor(repositoryOne)],
    approvals: [approvalFor(repositoryOne)],
  }],
  [repositoryTwo.repositoryId, {
    repository: repositoryTwo,
    deliveries: [deliveryFor(repositoryTwo, { status: 'verifying', openAttentionCount: 0 })],
    sessions: [],
    approvals: [],
  }],
  // A Scope that was never used: the dashboard must offer the first-use entry.
  [repositoryThree.repositoryId, {
    repository: repositoryThree,
    deliveries: [],
    sessions: [],
    approvals: [],
  }],
])

function serve(request) {
  const repositoryId = request.scope?.repositoryId
  const state = workspace.get(repositoryId)
  if (state === undefined) throw new Error(`unexpected scope ${String(repositoryId)}`)
  const result = () => {
    if (request.query === 'delivery.list') {
      return { kind: 'delivery_page', items: state.deliveries }
    }
    if (request.query === 'delivery.get') {
      const delivery = state.deliveries.find(
        item => item.deliveryId === request.parameters.deliveryId,
      )
      return delivery === undefined ? null : deliveryDetail(delivery)
    }
    if (request.query === 'session.list') {
      return { kind: 'product_session_page', items: state.sessions }
    }
    if (request.query === 'session.interactions.list') {
      return { kind: 'chat_interaction_page', items: [] }
    }
    if (request.query === 'approval.list') {
      return { kind: 'approval_page', items: state.approvals }
    }
    if (request.query === 'worker.list') {
      return {
        kind: 'worker_page',
        items: [{
          id: canonicalId('wrk', 1),
          state: 'enabled',
          capacity: 2,
          lastHeartbeatAt: new Date(Date.now() - 30_000).toISOString(),
          revision: 1,
        }],
      }
    }
    if (request.query === 'credential.reference.list') {
      return { kind: 'credential_reference_page', items: [] }
    }
    if (request.query === 'settings.get') {
      return { revision: 1, defaultModelRoute: null, workerConcurrencyLimit: 2 }
    }
    if (request.query === 'model.route.availability.list') {
      return {
        kind: 'model_route_availability_page',
        scope: request.scope,
        requestPoolSource: request.scope,
        requestPoolRevision: 1,
        settingsRevision: 1,
        settingsSource: request.scope,
        defaultProviderId: null,
        defaultModelId: null,
        status: 'disabled',
        reason: 'no_provider',
        items: [],
      }
    }
    throw new Error(`unexpected query: ${request.query}`)
  }
  return response(request, result())
}

const controlPlane = {
  serverUrl: 'https://control.localhost/home-dashboard',
  async restore() {
    return {
      schemaVersion,
      expiresAt: '2099-09-03T00:00:00.000Z',
      actor,
      authorizedScopes: [repositoryOne, repositoryTwo, repositoryThree],
    }
  },
  async login() {
    return {
      schemaVersion,
      expiresAt: '2099-09-03T00:00:00.000Z',
      actor,
      authorizedScopes: [repositoryOne, repositoryTwo, repositoryThree],
    }
  },
  async logout() {},
  async command() { throw new Error('unexpected command') },
  async query(request) {
    queries.push(structuredClone(request))
    return serve(request)
  },
  subscribe() {
    return { cursor: null, resume() {}, reconnect() {}, close() {} }
  },
  close() {},
}

const root = document.querySelector('[data-winwincode-client-root]')
mountWinWinCodeClient({ root, serverUrl: controlPlane.serverUrl, controlPlane })

function waitFor(predicate, label) {
  const deadline = Date.now() + 10_000
  return (async () => {
    while (!predicate()) {
      if (Date.now() >= deadline) throw new Error(`timed out waiting for ${label}`)
      await new Promise(resolvePromise => { setTimeout(resolvePromise, 20) })
    }
  })()
}

function descendants(node) {
  return [node, ...node.children.flatMap(child => descendants(child))]
}

function dashboard() {
  const page = document.querySelector('.wwc-home')
  if (page === null) {
    return {
      present: false,
      leak: document.body.textContent.includes(SECRET_MARKER)
        || (localStorage.getItem('winwincode.home.visits.v1') ?? '').includes(SECRET_MARKER),
    }
  }
  const cards = [...page.querySelectorAll('.wwc-home-card')]
  return {
    present: true,
    status: page.querySelector('.wwc-home-status')?.textContent ?? '',
    liveRegions: page.querySelectorAll('[aria-live="polite"]').length,
    politeRoles: page.querySelectorAll('[role="status"][aria-live="polite"]').length,
    sections: [...page.querySelectorAll('.wwc-home-section')].map(section => ({
      id: section.dataset.section,
      count: section.querySelector('.wwc-home-section-count')?.textContent ?? '',
      empty: section.querySelector('.wwc-home-section-empty')?.hidden ?? null,
      cards: [...section.querySelectorAll('.wwc-home-card')].map(card => ({
        kind: card.dataset.kind,
        status: card.dataset.status ?? null,
        disabled: card.dataset.disabled ?? null,
        title: card.querySelector('.wwc-home-card-title')?.textContent ?? '',
      })),
    })),
    actions: [...page.querySelectorAll('.wwc-home-card-action')].map(node => ({
      href: node.getAttribute('href'),
      disabled: node.getAttribute('aria-disabled'),
    })),
    chatLinks: [...page.querySelectorAll('.wwc-home-card-chat')]
      .filter(node => node.hidden !== true)
      .map(node => node.getAttribute('href')),
    firstUse: {
      hidden: document.querySelector('.wwc-home-first-use')?.hidden ?? null,
      links: [...(document.querySelector('.wwc-home-first-use')?.querySelectorAll('a') ?? [])]
        .map(node => node.getAttribute('href')),
    },
    usage: {
      present: page.querySelector('.wwc-usage-health') !== null,
      unavailable: [...page.querySelectorAll('.wwc-usage-health-unavailable')]
        .filter(node => node.hidden === false).length,
    },
    unavailableNotes: [...page.querySelectorAll('.wwc-home-unavailable')]
      .filter(node => node.hidden === false)
      .map(node => node.textContent),
    cardsRendered: cards.length,
    leak: document.body.textContent.includes(SECRET_MARKER)
      || localStorage.getItem('winwincode.home.visits.v1')?.includes(SECRET_MARKER)
      || JSON.stringify(sessionStorage).includes(SECRET_MARKER),
  }
}

globalThis.homeReady = () => true

globalThis.openHome = async path => {
  location.hash = path ?? '#/home'
  const repositoryId = (path ?? '').match(/repositoryId=(rep_[a-z0-9]+)/u)?.[1] ?? null
  await waitFor(() => {
    const next = dashboard()
    if (!next.present) return false
    if (repositoryId === null) return true
    // The dashboard of the requested Scope is on screen only once every card
    // link carries its repository identity.
    return next.firstUse.hidden === false
      || next.actions.every(action => (action.href ?? '').includes(repositoryId))
  }, 'dashboard of the requested Scope')
  await waitFor(
    () => dashboard().sections.some(section => section.cards.length > 0)
      || dashboard().firstUse.hidden === false,
    'dashboard cards',
  )
  return dashboard()
}

globalThis.inspectLanding = () => ({
  hash: location.hash,
  surface: document.querySelector('[data-winwincode-surface]')?.dataset.winwincodeSurface
    ?? document.querySelector('.wwc-surface-slot')?.dataset.winwincodeSurface
    ?? null,
  ...dashboard(),
})

globalThis.switchRepositoryScope = async () => {
  const before = dashboard()
  location.hash = `#/home?organizationId=${identity.organizationId}`
    + `&workspaceId=${identity.workspaceId}&projectId=${identity.projectId}`
    + `&repositoryId=${repositoryTwo.repositoryId}`
  await waitFor(() => {
    const next = dashboard()
    return next.present && next.actions.some(action => (action.href ?? '').includes(
      repositoryTwo.repositoryId,
    ))
  }, 'dashboard of the second Scope')
  const after = dashboard()
  return {
    before: before.actions,
    after: after.actions,
    beforeSectionTitles: before.sections.flatMap(section => section.cards.map(card => card.title)),
    afterSectionTitles: after.sections.flatMap(section => section.cards.map(card => card.title)),
    scopedQueries: queries
      .filter(request => request.query === 'delivery.list')
      .map(request => request.scope.repositoryId),
    leak: after.leak,
  }
}

globalThis.readRecentVisits = () => {
  const raw = localStorage.getItem('winwincode.home.visits.v1')
  if (raw === null) return { stored: false, entries: [] }
  const parsed = JSON.parse(raw)
  return { stored: true, entries: parsed.visits }
}

globalThis.readDashboard = dashboard

globalThis.waitUntil = async predicate => {
  await waitFor(predicate, 'browser condition')
  return true
}
globalThis.descendantCount = node => descendants(node).length
