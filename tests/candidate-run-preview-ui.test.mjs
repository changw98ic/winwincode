import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { pathToFileURL } from 'node:url'
import { resolve } from 'node:path'
import test from 'node:test'

import { findByClass, TrackedDocument } from './fixtures/ui601-keyed-dom.mjs'

const root = resolve(import.meta.dirname, '..')
const compiler = spawnSync(
  'corepack',
  [
    'pnpm',
    'exec',
    'tsc',
    '-p',
    'apps/client/tsconfig.candidate-run-preview-tests.json',
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
  `Candidate run preview area did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const cache = resolve(root, '.cache/candidate-run-preview-tests')
async function cachedModule(name) {
  return import(`${pathToFileURL(resolve(cache, name)).href}?run=${String(Date.now())}`)
}

const viewModule = await cachedModule('candidate-run-preview-view-model.js')
const pageModule = await cachedModule('candidate-run-preview-page.js')
const controlPlaneModule = await cachedModule('candidate-run-preview-control-plane.js')
const annotationModule = await cachedModule('page-annotation-view-model.js')

const {
  createCandidatePreviewViewModel,
  candidatePreviewModeText,
  candidatePreviewAcceptanceEligible,
  classifyPreviewFile,
  isCandidatePreviewIdentity,
  isSafePreviewOrigin,
  isSafePreviewPath,
  isRepositoryRelativePath,
  managedRunPhaseText,
  PREVIEW_LOG_MAX_LINES,
  projectLogSegments,
} = viewModule
const { mountCandidateRunPreviewPage } = pageModule
const { createControlPlaneCandidatePreviewPort } = controlPlaneModule
const { createPageAnnotationViewModel } = annotationModule

function flush() {
  return new Promise(resolvePromise => setTimeout(resolvePromise, 0))
}

function identity(overrides = {}) {
  return {
    mode: 'frozen-candidate',
    clientId: '123456789012',
    sourceId: 'pvs_demo',
    workerSessionId: 'ws_demo',
    repositoryBindingId: 'rb_demo',
    taskId: 'task_1',
    attempt: 1,
    candidateCommit: 'a'.repeat(40),
    candidateTreeId: 'b'.repeat(40),
    runConfigVersion: 'cfg-1',
    ...overrides,
  }
}

function runControl(overrides = {}) {
  return {
    runId: 'run_1',
    leaseId: 'lease_1',
    phase: 'starting',
    startedAt: '2026-09-12T00:00:00.000Z',
    exitedAt: null,
    exitCode: null,
    failureReason: null,
    ...overrides,
  }
}

function sourceFacts(overrides = {}) {
  return {
    previewAccessId: 'pva_demo',
    sourceId: 'pvs_demo',
    mode: 'frozen-candidate',
    candidateCommit: 'a'.repeat(40),
    previewOrigin: 'https://preview.example.test/p/pvs_demo',
    access: 'authorized',
    accessExpiresAt: '2026-09-12T00:10:00.000Z',
    ...overrides,
  }
}

function createPort(overrides = {}) {
  const calls = { start: 0, stop: 0, restart: 0, authorize: 0 }
  const port = {
    calls,
    async loadIdentity() {
      return identity()
    },
    async startRun() {
      calls.start += 1
      return runControl({ phase: 'ready' })
    },
    async stopRun() {
      calls.stop += 1
      return runControl({ phase: 'exited', exitedAt: '2026-09-12T00:05:00.000Z', exitCode: 0 })
    },
    async restartRun() {
      calls.restart += 1
      return runControl({ phase: 'ready' })
    },
    async authorizePreview() {
      calls.authorize += 1
      return sourceFacts()
    },
    async revokePreview() {
      return sourceFacts({ access: 'revoked', accessExpiresAt: null })
    },
    async listFiles() {
      return [
        classifyPreviewFile('README.md'),
        classifyPreviewFile('/etc/passwd'),
      ]
    },
    async listLogSegments() {
      return []
    },
    async readLogCitation() {
      return {
        segmentKey: 'stdout',
        lineStart: 1,
        lineEnd: 2,
        redactedText: '[redacted] ready',
        sourceRef: 'run_1#stdout:1-2',
      }
    },
    ...overrides,
  }
  return port
}

test('RUN-02 mode copy never presents live preview as acceptance proof', () => {
  assert.equal(candidatePreviewModeText('live'), '实时开发预览')
  assert.equal(candidatePreviewModeText('frozen-candidate'), '冻结候选验收预览')
  assert.equal(candidatePreviewAcceptanceEligible('live'), false)
  assert.equal(candidatePreviewAcceptanceEligible('frozen-candidate'), true)
})

test('RUN-02 frozen mode without a commit refuses start', async () => {
  const port = createPort({
    async loadIdentity() {
      return identity({ mode: 'frozen-candidate', candidateCommit: null })
    },
  })
  const model = createCandidatePreviewViewModel({
    port,
    nextRequestId: () => 'req_1',
  })
  await model.start()
  assert.equal(model.state.status, 'error')
  assert.match(model.state.notice ?? '', /身份不完整或无效/)
  assert.equal(isCandidatePreviewIdentity(identity()), true)
  assert.equal(isCandidatePreviewIdentity(identity({ attempt: Number.NaN })), false)
  assert.equal(isCandidatePreviewIdentity(identity({ mode: 'live' })), false)
})

test('RUN-03 start is idempotent for one live run', async () => {
  const port = createPort()
  const model = createCandidatePreviewViewModel({
    port,
    nextRequestId: () => 'req_1',
  })
  await model.start()
  await model.startRun()
  assert.equal(model.state.run?.phase, 'ready')
  assert.equal(port.calls.start, 1)
  await model.startRun()
  assert.equal(port.calls.start, 1)
  assert.match(model.state.notice ?? '', /不会重复启动/)
})

test('RUN-03 stop only targets the leased run', async () => {
  const port = createPort()
  const model = createCandidatePreviewViewModel({
    port,
    nextRequestId: () => 'req_1',
  })
  await model.start()
  await model.startRun()
  await model.stopRun()
  assert.equal(port.calls.stop, 1)
  assert.equal(model.state.run?.phase, 'exited')
})

test('RUN-05 rejects local or metadata preview origins', async () => {
  const port = createPort({
    async authorizePreview() {
      return sourceFacts({ previewOrigin: 'http://127.0.0.1:3000/' })
    },
  })
  const model = createCandidatePreviewViewModel({
    port,
    nextRequestId: () => 'req_1',
  })
  await model.start()
  await model.authorizePreview()
  assert.equal(model.state.source, null)
  assert.match(model.state.notice ?? '', /安全地址/)
  assert.equal(isSafePreviewOrigin('https://preview.example.test/p/x'), true)
  assert.equal(isSafePreviewOrigin('http://localhost:1234/'), false)
  assert.equal(isSafePreviewOrigin('http://169.254.169.254/'), false)
})

test('RUN-05 revoke clears files and marks access revoked', async () => {
  const port = createPort()
  const model = createCandidatePreviewViewModel({
    port,
    nextRequestId: () => 'req_1',
  })
  await model.start()
  await model.authorizePreview()
  assert.equal(model.state.files.length, 2)
  await model.revokePreview()
  assert.equal(model.state.source?.access, 'revoked')
  assert.equal(model.state.files.length, 0)
})

test('RUN-06 viewport presets and navigation stack', async () => {
  const port = createPort()
  const model = createCandidatePreviewViewModel({
    port,
    nextRequestId: () => 'req_1',
  })
  await model.start()
  model.setViewport('mobile')
  assert.equal(model.state.viewport.preset, 'mobile')
  assert.equal(model.state.viewport.width, 390)
  model.setViewport('custom', { width: 100, height: 50 })
  assert.equal(model.state.viewport.width, 200)
  model.navigate('/docs')
  model.navigate('/docs/a')
  model.goBack()
  assert.equal(model.state.navigation.path, '/docs')
  assert.equal(model.state.navigation.canGoForward, true)
  model.goForward()
  assert.equal(model.state.navigation.path, '/docs/a')
  model.navigate('relative')
  assert.match(model.state.navigation.lastError ?? '', /安全的站内绝对路径/)
  assert.equal(isSafePreviewPath('/docs?tab=one'), true)
  assert.equal(isSafePreviewPath('/../secret'), false)
  assert.equal(isSafePreviewPath('/%2e%2e/secret'), false)
})

test('RUN-07 repository-relative paths only; degraded paths are not openable', async () => {
  assert.equal(isRepositoryRelativePath('src/app.ts'), true)
  assert.equal(isRepositoryRelativePath('../secret'), false)
  assert.equal(isRepositoryRelativePath('/etc/passwd'), false)
  const markdown = classifyPreviewFile('docs/readme.md')
  assert.equal(markdown.previewClass, 'markdown')
  const html = classifyPreviewFile('public/index.html')
  assert.equal(html.previewClass, 'html')
  const binary = classifyPreviewFile('dist/app.bin')
  assert.equal(binary.previewClass, 'download')
  const denied = classifyPreviewFile('/etc/passwd')
  assert.equal(denied.previewClass, 'degraded')

  const port = createPort()
  const model = createCandidatePreviewViewModel({
    port,
    nextRequestId: () => 'req_1',
  })
  await model.start()
  model.openFile('/etc/passwd')
  assert.match(model.state.notice ?? '', /仓库相对位置/)
  assert.equal(model.state.navigation.path, '/')
})

test('RUN-08 log segments stay bounded and citations stay source-bound', async () => {
  assert.equal(managedRunPhaseText('ready'), '已就绪')
  const projected = projectLogSegments([
    { key: 'stdout', stream: 'stdout', lineCount: 500, truncated: false, maxLines: 1000 },
  ])
  assert.equal(projected[0].maxLines, PREVIEW_LOG_MAX_LINES)
  assert.equal(projected[0].truncated, true)

  const port = createPort()
  const model = createCandidatePreviewViewModel({
    port,
    nextRequestId: () => 'req_1',
  })
  await model.start()
  await model.startRun()
  await model.citeLog({ segmentKey: 'stdout', lineStart: 1, lineEnd: 2 })
  assert.equal(model.state.logCitation?.sourceRef, 'run_1#stdout:1-2')
})

test('page mounts mode, controls, viewport, and files from one snapshot', async () => {
  const document = new TrackedDocument()
  const rootElement = document.createElement('div')
  const port = createPort()
  const annotations = createPageAnnotationViewModel({ appOrigin: 'https://app.example.test' })
  const model = createCandidatePreviewViewModel({
    port,
    nextRequestId: () => 'req_1',
  })
  const page = mountCandidateRunPreviewPage({
    root: rootElement,
    model,
    annotations,
    appOrigin: 'https://app.example.test',
    devicePixelRatio: 2,
    taskHref: '#/home',
  })
  await flush()
  await flush()

  const mode = findByClass(rootElement, 'wwc-candidate-run-preview-mode')
  assert.ok(mode)
  assert.match(mode.textContent, /冻结候选验收预览/)
  const start = findByClass(rootElement, 'wwc-candidate-run-preview-start')
  assert.ok(start)
  start.emit('click')
  await flush()
  await flush()
  assert.equal(port.calls.start, 1)

  const authorize = findByClass(rootElement, 'wwc-candidate-run-preview-authorize')
  assert.ok(authorize)
  authorize.emit('click')
  await flush()
  await flush()

  const frame = findByClass(rootElement, 'wwc-candidate-run-preview-frame')
  assert.ok(frame)
  assert.equal(frame.getAttribute('sandbox'), 'allow-downloads allow-forms allow-modals allow-popups allow-scripts')
  assert.equal(frame.hidden, false)
  assert.match(frame.src, /^https:\/\/preview\.example\.test\/p\/pvs_demo/)
  const access = findByClass(rootElement, 'wwc-candidate-run-preview-access')
  assert.ok(access)
  assert.doesNotMatch(access.textContent, /preview\.example\.test/)
  const annotationMode = findByClass(rootElement, 'wwc-page-annotation-mode')
  assert.ok(annotationMode)
  assert.match(annotationMode.textContent, /降级为截图坐标/)
  assert.equal(annotations.state.status, 'ready')
  if (annotations.state.status === 'ready') {
    assert.equal(annotations.state.surfaceKind, 'screenshot-coordinate')
  }

  const degraded = findByClass(rootElement, 'wwc-candidate-run-preview-file-open')
  assert.ok(degraded)
  // First file is README.md (enabled); second is /etc/passwd (disabled).
  let openButton = degraded
  let sawEnabled = false
  let sawDisabled = false
  const walk = node => {
    if (node.className === 'wwc-candidate-run-preview-file-open') {
      if (node.disabled) sawDisabled = true
      else sawEnabled = true
    }
    for (const child of node.children) walk(child)
  }
  walk(rootElement)
  assert.equal(sawEnabled, true)
  assert.equal(sawDisabled, true)
  assert.equal(openButton.disabled, false)

  const mobile = findByClass(rootElement, 'wwc-candidate-run-preview-viewport-mobile')
  assert.ok(mobile)
  mobile.emit('click')
  const viewportLabel = findByClass(rootElement, 'wwc-candidate-run-preview-viewport-label')
  assert.match(viewportLabel.textContent, /mobile · 390×844/)

  page.close()
  assert.equal(rootElement.childNodes.length, 0)
})

test('RUN-05 browser port grants and revokes the exact Server preview access', async () => {
  const requests = []
  const port = createControlPlaneCandidatePreviewPort({
    serverUrl: 'https://server.example.test',
    identity: identity(),
    async fetch(url, init) {
      requests.push({ url, init })
      if (init.method === 'DELETE') return new Response(null, { status: 204 })
      return new Response(JSON.stringify({
        schemaVersion: 'winwincode/v1',
        previewAccessId: 'pva_0123456789abcdef0123456789abcdef',
        previewUrl: `https://server.example.test/p/pva_0123456789abcdef0123456789abcdef/${'b'.repeat(64)}/`,
        expiresAt: '2026-09-12T00:10:00.000Z',
        source: {
          sourceId: 'pvs_demo',
          workerSessionId: 'ws_demo',
          repositoryBindingId: 'rb_demo',
          mode: 'frozen-candidate',
          candidateCommit: 'a'.repeat(40),
        },
      }), { status: 201 })
    },
  })

  const granted = await port.authorizePreview({ sourceId: 'pvs_demo', requestId: 'req_1' })
  assert.equal(granted.previewAccessId, 'pva_0123456789abcdef0123456789abcdef')
  assert.deepEqual(JSON.parse(requests[0].init.body), {
    schemaVersion: 'winwincode/v1',
    clientId: '123456789012',
    sourceId: 'pvs_demo',
  })
  assert.equal(requests[0].init.credentials, 'include')
  await port.revokePreview({ previewAccessId: granted.previewAccessId, requestId: 'req_2' })
  assert.equal(requests[1].url, 'https://server.example.test/api/v1/previews/pva_0123456789abcdef0123456789abcdef')
})

test('RUN-05 browser port rejects a grant for a different source identity', async () => {
  const port = createControlPlaneCandidatePreviewPort({
    serverUrl: 'https://server.example.test',
    identity: identity(),
    async fetch() {
      return new Response(JSON.stringify({
        schemaVersion: 'winwincode/v1',
        previewAccessId: 'pva_0123456789abcdef0123456789abcdef',
        previewUrl: `https://server.example.test/p/pva_0123456789abcdef0123456789abcdef/${'b'.repeat(64)}/`,
        expiresAt: '2026-09-12T00:10:00.000Z',
        source: {
          sourceId: 'pvs_other',
          workerSessionId: 'ws_demo',
          repositoryBindingId: 'rb_demo',
          mode: 'frozen-candidate',
          candidateCommit: 'a'.repeat(40),
        },
      }), { status: 201 })
    },
  })

  await assert.rejects(
    port.authorizePreview({ sourceId: 'pvs_demo', requestId: 'req_1' }),
    error => error.code === 'INVALID_PREVIEW_ACCESS_RESPONSE',
  )
})
