import { mountWinWinCodeClient } from '/module/application.js'

const schemaVersion = 'winwincode/v1'
const actor = { kind: 'user', id: 'usr_00000000000000000000000001' }
const repositoryScope = {
  kind: 'repository',
  organizationId: 'org_00000000000000000000000001',
  workspaceId: 'wsp_00000000000000000000000001',
  projectId: 'prj_00000000000000000000000001',
  repositoryId: 'rep_00000000000000000000000001',
}
// Planted marker: if any secret-safe boundary leaks into the checklist, this string
// becomes readable in the page text and the browser assertion fails.
const SECRET_MARKER = 'vault-locator-secret-marker'
let complete = false
const queries = []

function session() {
  return {
    schemaVersion,
    expiresAt: '2099-09-03T00:00:00.000Z',
    actor,
    authorizedScopes: [repositoryScope],
  }
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

function ok(request) {
  if (request.query === 'enterprise.organization.list') return response(request, {
    kind: 'enterprise_organization_page',
    snapshotRevision: 1,
    items: [{
      id: repositoryScope.organizationId,
      displayName: 'Acme',
      slug: 'acme',
      state: 'active',
      revision: 1,
      updatedAt: '2026-09-03T00:00:00.000Z',
    }],
  })
  if (request.query === 'enterprise.project.list') return response(request, {
    kind: 'enterprise_project_repository_page',
    snapshotRevision: 1,
    items: [{
      kind: 'project',
      projectId: repositoryScope.projectId,
      displayName: 'Core',
      repositoryCount: 1,
      state: 'active',
      revision: 1,
      updatedAt: '2026-09-03T00:00:00.000Z',
    }, {
      kind: 'repository',
      projectId: repositoryScope.projectId,
      repositoryId: repositoryScope.repositoryId,
      displayName: 'Server',
      defaultBranch: 'main',
      state: 'active',
      revision: 1,
      updatedAt: '2026-09-03T00:00:00.000Z',
    }],
  })
  if (request.query === 'settings.get') return response(request, {
    revision: 1,
    defaultModelRoute: null,
    workerConcurrencyLimit: 2,
  })
  if (request.query === 'model.route.availability.list') return response(request, {
    kind: 'model_route_availability_page',
    scope: request.scope,
    requestPoolSource: {
      kind: 'project',
      organizationId: request.scope.organizationId,
      workspaceId: request.scope.workspaceId,
      projectId: request.scope.projectId,
    },
    requestPoolRevision: 1,
    settingsRevision: 1,
    settingsSource: request.scope,
    defaultProviderId: complete ? 'openai' : null,
    defaultModelId: complete ? 'gpt-5' : null,
    status: complete ? 'enabled' : 'disabled',
    reason: complete ? 'ready' : 'no_provider',
    items: complete
      ? [{
          route: {
            providerId: 'openai',
            modelId: 'gpt-5',
            credentialReferenceId: 'crd_00000000000000000000000001',
          },
          status: 'enabled',
          reason: 'ready',
          isDefault: true,
          providerDisplayName: 'OpenAI',
          modelDisplayName: 'GPT-5',
          contextWindowTokens: 400000,
          maxOutputTokens: 128000,
          reasoningEfforts: [],
          toolSupport: 'parallel',
          catalogSource: request.scope,
          catalogVersion: 1,
          credentialRotationVersion: 1,
          providerVersion: 1,
          modelVersion: 1,
        }]
      : [],
  })
  if (request.query === 'credential.reference.list') return response(request, {
    kind: 'credential_reference_page',
    items: complete
      ? [{
          id: 'crd_00000000000000000000000001',
          displayName: 'Primary provider key',
          providerId: 'openai',
          secretState: 'available',
          revokedAt: null,
          lastRotatedAt: '2026-09-01T00:00:00.000Z',
          rotationVersion: 1,
          revision: 1,
          updatedAt: '2026-09-01T00:00:00.000Z',
          [SECRET_MARKER]: true,
        }]
      : [],
  })
  if (request.query === 'worker.list') return response(request, {
    kind: 'worker_page',
    items: complete
      ? [{
          id: 'wrk_00000000000000000000000001',
          state: 'enabled',
          capacity: 2,
          lastHeartbeatAt: '2026-09-03T08:00:00.000Z',
          revision: 2,
        }]
      : [],
  })
  if (request.query === 'session.list') return response(request, {
    kind: 'product_session_page',
    items: complete
      ? [{
          id: 'psn_00000000000000000000000001',
          title: 'First Chat',
          projectId: repositoryScope.projectId,
          repositoryId: repositoryScope.repositoryId,
          state: 'active',
          revision: 1,
          createdAt: '2026-09-03T08:00:00.000Z',
          updatedAt: '2026-09-03T08:00:00.000Z',
        }]
      : [],
  })
  if (request.query === 'delivery.list') return response(request, {
    kind: 'delivery_page',
    items: complete
      ? [{
          deliveryId: 'dlv_00000000000000000000000001',
          title: 'First Delivery',
          projectId: repositoryScope.projectId,
          repositoryId: repositoryScope.repositoryId,
          state: 'draft',
          revision: 1,
          createdAt: '2026-09-03T08:00:00.000Z',
          updatedAt: '2026-09-03T08:00:00.000Z',
        }]
      : [],
  })
  throw new Error(`unexpected query: ${request.query}`)
}

const controlPlane = {
  serverUrl: 'https://control.localhost/readiness',
  async restore() { return structuredClone(session()) },
  async login() { return structuredClone(session()) },
  async logout() {},
  async command() { throw new Error('unexpected command') },
  async query(request) {
    queries.push(structuredClone(request))
    return ok(request)
  },
  subscribe() {
    return { cursor: null, resume() {}, reconnect() {}, close() {} }
  },
  close() {},
}

const root = document.querySelector('[data-winwincode-client-root]')
mountWinWinCodeClient({
  root,
  serverUrl: controlPlane.serverUrl,
  controlPlane,
})

async function waitFor(predicate, label) {
  const deadline = Date.now() + 5_000
  while (Date.now() < deadline) {
    if (predicate()) return
    await new Promise(resolvePromise => { setTimeout(resolvePromise, 20) })
  }
  throw new Error(`timed out waiting for ${label}`)
}

function section() {
  return document.querySelector('.wwc-readiness')
}

function items() {
  return [...document.querySelectorAll('.wwc-readiness-item')]
}

function click(className) {
  const node = document.querySelector(`.${className}`)
  if (node === null) throw new Error(`missing .${className}`)
  node.click()
}

function inspect() {
  const root2 = section()
  if (root2 === null) return { present: false }
  return {
    present: true,
    hidden: root2.closest('.wwc-readiness-root')?.hidden ?? false,
    summary: root2.querySelector('.wwc-readiness-summary')?.textContent ?? '',
    expanded: root2.querySelector('.wwc-readiness-toggle')?.getAttribute('aria-expanded') === 'true',
    itemsHidden: root2.querySelector('.wwc-readiness-items')?.hidden ?? null,
    items: items().map(item => {
      const fix = item.querySelector('.wwc-readiness-fix')
      return {
        id: item.dataset.itemId,
        status: item.dataset.status,
        reason: item.querySelector('.wwc-readiness-item-reason')?.textContent ?? '',
        checkedAt: item.querySelector('.wwc-readiness-item-checked')?.textContent ?? null,
        fixHref: fix?.getAttribute('href') ?? null,
        fixLabel: fix?.textContent ?? null,
      }
    }),
    leak: document.body.textContent.includes(SECRET_MARKER)
      || document.body.textContent.includes('crd_00000000000000000000000001'),
  }
}

globalThis.readinessReady = () => true

globalThis.inspectChecklist = () => inspect()

globalThis.completeAllSteps = async () => {
  complete = true
  click('wwc-readiness-recheck')
  await waitFor(() => inspect().summary.includes('6 of 6 complete'), 'complete checklist')
  return inspect()
}

globalThis.collapseChecklist = async () => {
  click('wwc-readiness-toggle')
  await waitFor(() => inspect().itemsHidden === true, 'collapsed checklist')
  return inspect()
}

globalThis.clickModelRouteFix = async () => {
  const modelFix = items()
    .find(item => item.dataset.itemId === 'model-route')
    ?.querySelector('.wwc-readiness-fix')
  if (modelFix === null || modelFix === undefined) throw new Error('missing model route fix link')
  modelFix.click()
  await waitFor(() => location.hash.includes('repositoryId='), 'scope-encoded settings link')
  return { hash: location.hash }
}

globalThis.openFromDiagnostics = async () => {
  location.hash = '#/settings/runtime'
  await waitFor(() => document.querySelector('.wwc-local-operations') !== null, 'diagnostics page')
  await waitFor(() => inspect().present, 'checklist on diagnostics page')
  const reopen = document.querySelector('.wwc-local-readiness-open')
  if (reopen === null) throw new Error('missing diagnostics reopen entry')
  reopen.click()
  await waitFor(() => inspect().itemsHidden === false, 'reopened checklist')
  return { ...inspect(), reopenPresent: true }
}
