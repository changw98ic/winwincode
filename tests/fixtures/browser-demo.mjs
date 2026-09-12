// Community-UI demo fixture: mounts the one browser shell over a deterministic
// Control Plane facade whose sample data matches the design mockups
// (docs/design/community-ui-geometric).  The 执行设备/项目 pages read the real
// `/api/v1/clients` and `/api/v1/repositories` endpoints, which the local demo
// server serves with the same design sample devices and repositories.
//
// Determinism: the shell receives a fixed clock, every served timestamp and
// identifier is a literal, the font stack is pinned, and transitions,
// animations, and scrollbars are switched off.

import { mountWinWinCodeClient } from '/module/application.js'
import { ControlPlaneClientError } from '/module/community-control-plane-client.js'

const DETERMINISM_CSS = `
*, *::before, *::after {
  animation: none !important;
  transition: none !important;
  caret-color: transparent !important;
}
html { scrollbar-width: none; }
::-webkit-scrollbar { display: none; }
`

const FIXED_NOW = '2026-09-02T01:00:00.000Z'

const schemaVersion = 'winwincode/v1'
const actor = { kind: 'user', id: 'usr_00000000000000000000000001' }
const identity = {
  organizationId: 'org_00000000000000000000000001',
  workspaceId: 'wsp_00000000000000000000000001',
  projectId: 'prj_00000000000000000000000001',
}
const scope = {
  kind: 'repository',
  ...identity,
  repositoryId: 'rep_00000000000000000000000001',
}
const productSessionId = 'psn_00000000000000000000000001'
const credentialReferenceId = 'crd_00000000000000000000000001'
const modelRoute = {
  providerId: 'zhipu-provider',
  modelId: 'glm-5.3',
  credentialReferenceId,
}

function canonicalId(prefix, value) {
  return `${prefix}_${String(value).padStart(26, '0')}`
}

/** 设计稿 03a 空对话:load-time hash 里带 chat-empty 时,不提供任何会话。 */
const emptyChat = location.hash.includes('chat-empty')
/** 设计稿 01 登录页:URL 带 mode=login 时不恢复会话,展示登录页。 */
const loginMode = location.search.includes('mode=login')

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

function ownership() {
  return { ...identity, repositoryId: scope.repositoryId }
}

function deliverySummary(index, overrides = {}) {
  return {
    deliveryId: canonicalId('dlv', index),
    revision: 4,
    schemaVersion,
    status: 'in_progress',
    title: `演示交付 ${String(index)}`,
    updatedAt: '2026-09-02T00:30:00.000Z',
    ownership: ownership(),
    activeWorkRunId: canonicalId('str', index),
    openAttentionCount: 0,
    workItemCounts: { ready: 0, waitingHuman: 0, candidateReady: 0, rework: 0, cancelled: 0, total: 4, backlog: 0, inProgress: 1, waitingDependency: 0, validating: 0, done: 1, failed: 2 },
    ...overrides,
  }
}

// 设计稿 04 任务看板:两列实时卡片 + 折叠的 未开始/已完成 历史行。
const deliveries = [
  // 设计稿 04:待我处理两卡的来源(审批 + 交付注意点)。
  deliverySummary(1, {
    title: '修复登录回跳',
    status: 'validating',
    activeWorkRunId: canonicalId('str', 1),
    workItemCounts: { ready: 0, waitingHuman: 0, candidateReady: 0, rework: 0, cancelled: 0, total: 3, backlog: 1, inProgress: 1, waitingDependency: 0, validating: 0, done: 1, failed: 0 },
  }),
  deliverySummary(2, {
    title: '导出筛选结果',
    status: 'candidate_ready',
    openAttentionCount: 1,
    workItemCounts: { ready: 0, waitingHuman: 0, candidateReady: 0, rework: 0, cancelled: 0, total: 3, backlog: 0, inProgress: 0, waitingDependency: 0, validating: 1, done: 2, failed: 0 },
  }),
  // 设计稿 04:正在运行两卡。
  deliverySummary(3, {
    title: '配置迁移',
    status: 'validating',
    workItemCounts: { ready: 0, waitingHuman: 0, candidateReady: 0, rework: 0, cancelled: 0, total: 4, backlog: 0, inProgress: 1, waitingDependency: 0, validating: 2, done: 1, failed: 0 },
  }),
  deliverySummary(4, {
    title: '修复缓存失效',
    status: 'in_progress',
    workItemCounts: { ready: 0, waitingHuman: 0, candidateReady: 0, rework: 0, cancelled: 0, total: 4, backlog: 0, inProgress: 2, waitingDependency: 0, validating: 0, done: 1, failed: 0 },
  }),
  ...Array.from({ length: 18 }, (_, index) => deliverySummary(100 + index, {
    title: `存档任务 ${String(index + 1)}`,
    status: 'done',
    activeWorkRunId: null,
    updatedAt: '2026-09-01T00:30:00.000Z',
    workItemCounts: { ready: 0, waitingHuman: 0, candidateReady: 0, rework: 0, cancelled: 0, total: 4, backlog: 0, inProgress: 0, waitingDependency: 0, validating: 0, done: 4, failed: 0 },
  })),
]

// 设计稿 04:待我处理列 = 修复登录回跳(审核方案)+ 导出筛选结果(验收交付)。
function approval(index) {
  return {
    id: canonicalId('apr', index),
    revision: 5,
    state: 'pending',
    requestedAt: '2026-09-02T00:40:00.000Z',
    expiresAt: '2099-09-02T00:00:00.000Z',
    subject: '修复登录回跳',
    binding: {
      productSessionId,
      executionJobId: canonicalId('job', index),
      workerSessionId: canonicalId('wss', index),
      sessionIdentity: {
        productSessionId,
        workerSessionId: canonicalId('wss', index),
        codexThreadId: canonicalId('thr', index),
        workRunId: canonicalId('wrn', index),
      },
    },
  }
}

function attentionItem(index, title) {
  return {
    id: canonicalId('att', index),
    workRunId: canonicalId('wrn', index),
    status: 'open',
    blocking: false,
    title,
    createdAt: '2026-09-02T00:30:00.000Z',
  }
}

let activeDemoSession = {
  id: productSessionId,
  projectId: scope.projectId,
  repositoryId: scope.repositoryId,
  revision: 3,
  state: 'idle',
  title: '登录问题讨论',
  updatedAt: '2026-09-02T00:30:00.000Z',
}

function chatSession() {
  return activeDemoSession
}

let demoMessages = []

function chatMessages() {
  return demoMessages
}

function appendDemoMessage(role, content) {
  demoMessages = demoMessages.concat({
    id: canonicalId('msg', demoMessages.length + 1),
    productSessionId: activeDemoSession.id,
    role,
    content,
    sequence: demoMessages.length + 1,
    state: 'completed',
    createdAt: FIXED_NOW,
    updatedAt: FIXED_NOW,
  })
}

function seedDemoMessages() {
  demoMessages = []
  appendDemoMessage('user', '修复登录成功后无法回跳的问题，按强流程推进。')
  appendDemoMessage('assistant', '已委托 #126「修复登录回跳」，方案正在等待审核。')
  appendDemoMessage('user', '我们再讨论一下导出功能的交互。')
}
seedDemoMessages()


function worker() {
  return {
    id: canonicalId('wrk', 1),
    state: 'enabled',
    capacity: 2,
    lastHeartbeatAt: '2026-09-02T00:59:00.000Z',
    revision: 1,
  }
}

function credentialReference() {
  return {
    id: credentialReferenceId,
    providerId: modelRoute.providerId,
    displayName: '默认模型凭据',
    secretState: 'available',
    rotationVersion: 1,
    lastRotatedAt: '2026-09-02T00:00:00.000Z',
    revokedAt: null,
    revision: 1,
    updatedAt: '2026-09-02T00:00:00.000Z',
  }
}

function routeAvailability() {
  return {
    kind: 'model_route_availability_page',
    scope,
    settingsSource: scope,
    settingsRevision: 1,
    requestPoolSource: { kind: 'project', ...identity },
    requestPoolRevision: 1,
    defaultProviderId: modelRoute.providerId,
    defaultModelId: modelRoute.modelId,
    status: 'enabled',
    reason: 'ready',
    items: [{
      route: modelRoute,
      providerDisplayName: '智谱',
      modelDisplayName: 'GLM-5.3',
      catalogSource: scope,
      catalogVersion: 1,
      providerVersion: 1,
      modelVersion: 1,
      contextWindowTokens: 128_000,
      maxOutputTokens: 16_000,
      toolSupport: 'parallel',
      reasoningEfforts: ['medium', 'high'],
      credentialRotationVersion: 1,
      isDefault: true,
      status: 'enabled',
      reason: 'ready',
    }],
  }
}

function chatRuntime() {
  return {
    kind: 'runtime_projection',
    productSessionId: activeDemoSession.id,
    deliveryId: null,
    workRunId: null,
    readCursor: null,
    eventCursor: {
      eventId: null,
      sequence: 0,
      scope,
      stream: { kind: 'product-session', productSessionId: activeDemoSession.id },
    },
    lastProjectionSequence: 0,
    revision: 1,
    rebuiltAt: '2026-09-02T00:31:00.000Z',
    sessions: [],
  }
}

function deliveryDetail(delivery) {
  return {
    deliveryId: delivery.deliveryId,
    deliveryRevision: delivery.revision,
    ownership: delivery.ownership,
    attention: delivery.openAttentionCount > 0
      ? [attentionItem(2, '导出筛选结果')]
      : [],
    currentCandidate: null,
    requirements: {
      repository: { kind: 'local-git', locator: 'workspace://repository' },
    },
    internalToolPayload: null,
  }
}

/** Every query a failing route needs in order to fail. */
function serve(request) {
  const result = () => {
    if (request.query === 'delivery.list') {
      return { kind: 'delivery_page', items: deliveries }
    }
    if (request.query === 'delivery.get') {
      const delivery = deliveries.find(
        item => item.deliveryId === request.parameters.deliveryId,
      )
      return delivery === undefined ? null : deliveryDetail(delivery)
    }
    if (request.query === 'enterprise.organization.list') {
      return {
        kind: 'organization_page',
        items: [{
          kind: 'organization',
          id: identity.organizationId,
          displayName: '个人组织',
          state: 'active',
        }],
        snapshotRevision: 1,
      }
    }
    if (request.query === 'enterprise.project.list') {
      return {
        kind: 'project_page',
        items: [
          { kind: 'project', projectId: identity.projectId, displayName: 'winwincode', state: 'active' },
          {
            kind: 'repository',
            repositoryId: scope.repositoryId,
            displayName: 'winwincode',
            state: 'active',
          },
        ],
        snapshotRevision: 1,
      }
    }
    if (request.query.startsWith('enterprise.') && request.query.endsWith('.list')) {
      return { kind: 'enterprise_projection_page', items: [], snapshotRevision: 1 }
    }
    if (request.query === 'session.list') {
      return { kind: 'product_session_page', items: emptyChat ? [] : [chatSession()] }
    }
    if (request.query === 'session.get') return emptyChat ? null : chatSession()
    if (request.query === 'session.messages.list') {
      return { kind: 'chat_message_page', items: chatMessages() }
    }
    if (request.query === 'session.interactions.list') {
      return { kind: 'chat_interaction_page', items: [] }
    }
    if (request.query === 'approval.list') {
      return { kind: 'approval_page', items: [approval(1)] }
    }
    if (request.query === 'worker.list') {
      return { kind: 'worker_page', items: [worker()] }
    }
    if (request.query === 'credential.reference.list') {
      return { kind: 'credential_reference_page', items: [credentialReference()] }
    }
    if (request.query === 'settings.get') {
      return {
        revision: 1,
        defaultModelRoute: modelRoute,
        workerConcurrencyLimit: 2,
      }
    }
    if (request.query === 'model.route.availability.list') return routeAvailability()
    if (request.query === 'runtime.projection.get') return chatRuntime()
    throw new Error(`unexpected query: ${request.query}`)
  }
  return response(request, result())
}

const controlPlane = {
  serverUrl: 'http://127.0.0.1:8080',
  async restore() {
    if (loginMode) {
      throw new ControlPlaneClientError({
        kind: 'authentication',
        code: 'AUTHENTICATION_REQUIRED',
        message: 'demo: signed out on purpose',
        requestId: null,
        retryable: false,
      })
    }
    return {
      schemaVersion,
      expiresAt: '2099-09-02T00:00:00.000Z',
      actor,
      authorizedScopes: [scope],
    }
  },
  async login() {
    return {
      schemaVersion,
      expiresAt: '2099-09-02T00:00:00.000Z',
      actor,
      authorizedScopes: [scope],
    }
  },
  async logout() {},
  async command(request) {
    if (request.command === 'session.create') {
      activeDemoSession = {
        id: request.payload.productSessionId,
        projectId: scope.projectId,
        repositoryId: scope.repositoryId,
        revision: 1,
        state: 'idle',
        title: request.payload.title,
        updatedAt: FIXED_NOW,
      }
      demoMessages = []
      return {
        schemaVersion,
        requestId: request.requestId,
        command: 'session.create',
        outcome: 'completed',
        previousRevision: 0,
        currentRevision: 1,
        result: activeDemoSession,
      }
    }
    if (request.command === 'chat.submit') {
      appendDemoMessage('user', request.payload.message)
      activeDemoSession = {
        ...activeDemoSession,
        revision: activeDemoSession.revision + 1,
        state: 'running',
        updatedAt: FIXED_NOW,
      }
      return {
        schemaVersion,
        requestId: request.requestId,
        command: 'chat.submit',
        outcome: 'completed',
        previousRevision: activeDemoSession.revision - 1,
        currentRevision: activeDemoSession.revision,
        result: activeDemoSession,
      }
    }
    throw new Error(`unexpected command: ${String(request.command)}`)
  },
  async query(request) {
    // serve() already applies the response envelope; wrapping again would hide
    // `result.items` behind a second envelope and fail every model read.
    return serve(request)
  },
  subscribe() {
    return { cursor: null, resume() {}, reconnect() {}, close() {} }
  },
  close() {},
}

// Determinism controls are installed through CSSOM because the harness serves a
// `style-src 'self'` policy, and the clock is pinned on the shell itself.
const determinism = new CSSStyleSheet()
determinism.replaceSync(DETERMINISM_CSS)
document.adoptedStyleSheets = [...document.adoptedStyleSheets, determinism]
for (const token of ['--wwc-font-family', '--wwc-font-family-mono']) {
  document.documentElement.style.setProperty(token, (
    "'PingFang SC', 'Hiragino Sans GB', 'Microsoft YaHei', system-ui, sans-serif"
  ))
}

// 设计稿侧栏「最近对话」: seeded browser-local titles (登录问题讨论 / 架构调整讨论).
try {
  globalThis.localStorage?.setItem('winwincode.recentChats.v1', JSON.stringify([
    { sessionKey: 'psn_00000000000000000000000001', title: '登录问题讨论', at: 1_787_000_000_000 },
    { sessionKey: 'demo-arch-chat', title: '架构调整讨论', at: 1_786_900_000_000 },
  ]))
} catch {
  /* best-effort seeding only */
}

// 设计稿 05 任务详情的演示锚点(假优先任务端口种子)。
mountWinWinCodeClient({
  root: document.querySelector('[data-winwincode-client-root]'),
  serverUrl: controlPlane.serverUrl,
  controlPlane,
  now: () => FIXED_NOW,
  taskSeed: [{
    taskId: 'tsk_00000000000000000000000001',
    clientId: '00000000000000000000000001',
    repositoryBindingId: 'rbn_000000000000000000000DEM01',
    baseBranch: 'main',
    title: '修复登录回跳',
    description: '修复登录后的回跳逻辑，保留现有鉴权方式。',
    changes: '改动：登录状态、回跳路由。',
    acceptance: '验收：成功登录返回原页面；失败登录不跳转。',
    modelRouteId: '',
  }],
})

globalThis.describeDemoViewport = () => ({
  width: document.documentElement.clientWidth,
  height: document.documentElement.clientHeight,
})
