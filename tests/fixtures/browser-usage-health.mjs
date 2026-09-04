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
// Planted markers: if any secret-unsafe identifier reaches the summary these
// strings become readable page text and the browser assertion fails.
const SECRET_MARKER = 'vault-locator-secret-marker'
const CREDENTIAL_MARKER = 'crd_00000000000000000000000001'
let complete = true
const queries = []
let blockedQuery = null
// Heartbeats and observation instants are published relative to the real browser
// clock so staleness classification observes the same facts in every run.
const FRESH_HEARTBEAT = new Date(Date.now() - 30_000).toISOString()
const RECENT_HEARTBEAT = new Date(Date.now() - 60_000).toISOString()
const ROTATED_AT = new Date(Date.now() - 48 * 3_600_000).toISOString()
const UPDATED_AT = new Date(Date.now() - 3_600_000).toISOString()

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

function runtimeSession(stageRunId, workerSessionId, overrides = {}) {
  return {
    activities: [],
    agentEdges: [],
    agents: [{
      nickname: 'planner',
      parentThreadId: null,
      path: null,
      role: 'planner',
      sourceRef: 'runtime:agent-1',
      status: 'completed',
      threadId: 'thr_00000000000000000000000001',
    }],
    asOfSequence: 12,
    attempt: 1,
    codexThreadId: 'thr_00000000000000000000000001',
    deliveryTaskId: null,
    diffSummary: null,
    executionJobId: 'job_00000000000000000000000001',
    fencingToken: '1',
    leaseId: 'lse_00000000000000000000000001',
    plan: null,
    productSessionId: 'psn_00000000000000000000000001',
    recovery: {
      failureCount: 0,
      lastFailureSourceRef: null,
      latestRecoverySourceRef: null,
      recoveryCount: 0,
      state: 'none',
    },
    sessionBindingId: 'binding-1',
    stageRunId,
    usage: {
      sourceRef: 'runtime:usage-1',
      totals: [
        { name: 'input_tokens', value: 120 },
        { name: 'output_tokens', value: 60 },
        { name: 'total_tokens', value: 180 },
      ],
    },
    workerSessionId,
    ...overrides,
  }
}

function ok(request) {
  if (request.query === blockedQuery) throw new Error(`blocked read: ${request.query}`)
  if (request.query === 'settings.get') return response(request, {
    revision: 4,
    defaultModelRoute: null,
    workerConcurrencyLimit: 2,
  })
  if (request.query === 'session.list') return response(request, {
    kind: 'product_session_page',
    items: complete
      ? [{
          id: 'psn_00000000000000000000000001',
          projectId: repositoryScope.projectId,
          repositoryId: repositoryScope.repositoryId,
          revision: 2,
          state: 'running',
          title: 'First Chat',
          updatedAt: UPDATED_AT,
        }]
      : [],
  })
  if (request.query === 'runtime.projection.get') {
    if (request.parameters.kind !== 'product-session') throw new Error('unexpected runtime read')
    return response(request, {
      deliveryId: 'dlv_00000000000000000000000001',
      eventCursor: {
        deliveryId: 'dlv_00000000000000000000000001',
        eventId: 'evt_00000000000000000000000001',
        kind: 'delivery',
        sequence: 12,
        stageRunId: 'str_00000000000000000000000001',
        stream: { kind: 'delivery' },
      },
      kind: 'runtime_projection',
      lastProjectionSequence: 12,
      productSessionId: request.parameters.productSessionId,
      readCursor: null,
      rebuiltAt: UPDATED_AT,
      revision: 3,
      stageRunId: 'str_00000000000000000000000001',
      sessions: complete
        ? [
            runtimeSession('str_00000000000000000000000001', 'wss_00000000000000000000000001'),
            runtimeSession(
              'str_00000000000000000000000002',
              'wss_00000000000000000000000002',
              {
                agents: [{
                  nickname: null,
                  parentThreadId: null,
                  path: null,
                  role: null,
                  sourceRef: 'runtime:agent-2',
                  status: 'failed',
                  threadId: 'thr_00000000000000000000000002',
                }],
                recovery: {
                  failureCount: 2,
                  lastFailureSourceRef: 'runtime:failure-2',
                  latestRecoverySourceRef: 'runtime:recovery-3',
                  recoveryCount: 1,
                  state: 'recovered',
                },
                usage: null,
              },
            ),
          ]
        : [],
    })
  }
  if (request.query === 'delivery.list') return response(request, {
    kind: 'delivery_page',
    items: complete
      ? [{
          activeStageRunId: 'str_00000000000000000000000001',
          deliveryId: 'dlv_00000000000000000000000001',
          openAttentionCount: 1,
          ownership: {
            organizationId: repositoryScope.organizationId,
            projectId: repositoryScope.projectId,
            repositoryId: repositoryScope.repositoryId,
            workspaceId: repositoryScope.workspaceId,
          },
          revision: 5,
          schemaVersion,
          status: 'executing',
          taskCounts: {
            active: 1,
            blocked: 0,
            completed: 0,
            failed: 0,
            pending: 0,
            total: 1,
            verifying: 0,
          },
          title: 'First Delivery',
          updatedAt: UPDATED_AT,
        }]
      : [],
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
    settingsRevision: 4,
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
            credentialReferenceId: CREDENTIAL_MARKER,
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
        }, {
          route: {
            providerId: 'mistral',
            modelId: 'mistral-large',
            credentialReferenceId: CREDENTIAL_MARKER,
          },
          status: 'enabled',
          reason: 'request_pool_unavailable',
          isDefault: false,
          providerDisplayName: 'Mistral',
          modelDisplayName: 'Mistral Large',
          contextWindowTokens: 128000,
          maxOutputTokens: 32000,
          reasoningEfforts: [],
          toolSupport: 'serial',
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
          displayName: 'Primary provider key',
          id: CREDENTIAL_MARKER,
          lastRotatedAt: ROTATED_AT,
          providerId: 'openai',
          revokedAt: null,
          rotationVersion: 3,
          secretState: 'available',
          updatedAt: ROTATED_AT,
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
          lastHeartbeatAt: FRESH_HEARTBEAT,
          revision: 2,
        }, {
          id: 'wrk_00000000000000000000000002',
          state: 'draining',
          capacity: 1,
          lastHeartbeatAt: RECENT_HEARTBEAT,
          revision: 2,
        }, {
          id: 'wrk_00000000000000000000000003',
          state: 'offline',
          capacity: 0,
          lastHeartbeatAt: null,
          revision: 2,
        }]
      : [],
  })
  throw new Error(`unexpected query: ${request.query}`)
}

const controlPlane = {
  serverUrl: 'https://control.localhost/usage-health',
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

globalThis.usageHealthReady = () => true

function summary() {
  const panel = document.querySelector('.wwc-usage-health')
  if (panel === null) return { present: false }
  const leak = document.body.textContent.includes(SECRET_MARKER)
    || document.body.textContent.includes(CREDENTIAL_MARKER)
  return {
    present: true,
    heading: panel.querySelector('.wwc-usage-health-heading')?.textContent ?? '',
    updated: panel.querySelector('.wwc-usage-health-updated')?.textContent ?? '',
    window: panel.querySelector('.wwc-usage-health-window')?.textContent ?? '',
    capacityState: panel.querySelector('.wwc-usage-health-capacity')?.dataset.capacityState ?? null,
    liveRegions: document.querySelectorAll('.wwc-usage-health [aria-live="polite"]').length,
    deliveries: [...panel.querySelectorAll('.wwc-usage-health-delivery')].map(row => ({
      key: row.dataset.key,
      usage: row.querySelector('.wwc-usage-health-row-usage')?.textContent ?? '',
    })),
    stageRuns: [...panel.querySelectorAll('.wwc-usage-health-stage-run')].map(row => ({
      key: row.dataset.key,
      unknown: row.dataset.unknown,
    })),
    providers: [...panel.querySelectorAll('.wwc-usage-health-provider')].map(row => ({
      key: row.dataset.key,
      state: row.dataset.providerState,
    })),
    models: [...panel.querySelectorAll('.wwc-usage-health-model')].length,
    workers: [...panel.querySelectorAll('.wwc-usage-health-worker')].map(row => ({
      state: row.dataset.workerState,
      tone: row.dataset.tone,
      label: row.querySelector('.wwc-usage-health-worker-state')?.textContent ?? '',
    })),
    credentials: [...panel.querySelectorAll('.wwc-usage-health-credential')].map(row =>
      row.querySelector('.wwc-usage-health-credential-state')?.textContent ?? ''),
    errors: [...panel.querySelectorAll('.wwc-usage-health-error')].map(row =>
      row.querySelector('.wwc-usage-health-error-detail')?.textContent ?? ''),
    unknownMarkers: [...panel.querySelectorAll('.wwc-usage-health-unknown')].map(
      node => node.dataset.unknown,
    ),
    leak,
  }
}

globalThis.inspectUsageHealth = () => summary()

globalThis.openDiagnosticsUsageHealth = async () => {
  location.hash = '#/settings/runtime'
  await waitFor(() => document.querySelector('.wwc-local-operations') !== null, 'diagnostics page')
  await waitFor(() => summary().present, 'usage and health summary')
  await waitFor(() => summary().deliveries.length > 0, 'usage rows')
  return summary()
}

globalThis.blockProviderRead = async () => {
  blockedQuery = 'model.route.availability.list'
  document.querySelector('.wwc-usage-health-refresh').click()
  await waitFor(() => summary().providers.length === 0, 'provider section marked unavailable')
  const panel = document.querySelector('.wwc-usage-health')
  return {
    providers: [...panel.querySelectorAll('.wwc-usage-health-provider')].length,
    models: [...panel.querySelectorAll('.wwc-usage-health-model')].length,
    providerSectionUnavailable: [...panel.querySelectorAll('.wwc-usage-health-unavailable')]
      .some(node => node.hidden === false
        && /This section is unavailable/u.test(node.textContent)),
    visibleUnavailableNotes: [...panel.querySelectorAll('.wwc-usage-health-unavailable')]
      .filter(node => node.hidden === false).length,
    deliveryRowsStillPresent: panel.querySelectorAll('.wwc-usage-health-delivery').length > 0,
    leak: document.body.textContent.includes(SECRET_MARKER)
      || document.body.textContent.includes(CREDENTIAL_MARKER),
  }
}

globalThis.refreshUsageHealth = async () => {
  document.querySelector('.wwc-usage-health-refresh').click()
  await waitFor(() => summary().deliveries.length > 0, 'refreshed usage rows')
  return summary()
}
