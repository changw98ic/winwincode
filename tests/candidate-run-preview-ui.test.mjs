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
    workRunId: 'wrn_demo',
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
    async readFileContent(input) {
      const data = Buffer.from('const answer = 42')
      return {
        path: input.path,
        mediaType: 'text/plain',
        contentEncoding: 'utf-8',
        dataBase64: data.toString('base64'),
        offset: input.offset,
        returnedBytes: data.length,
        totalBytes: data.length,
        nextOffset: null,
      }
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
  await model.startRun()
  assert.equal(port.calls.start, 0)
  assert.match(model.state.notice ?? '', /拒绝启动/)
  assert.equal(isCandidatePreviewIdentity(identity()), true)
  assert.equal(isCandidatePreviewIdentity(identity({ candidateCommit: 'a'.repeat(41), candidateTreeId: 'b'.repeat(64) })), false)
  assert.equal(isCandidatePreviewIdentity(identity({ candidateCommit: 'a'.repeat(64), candidateTreeId: 'b'.repeat(64) })), true)
  assert.equal(isCandidatePreviewIdentity(identity({ candidateCommit: 'A'.repeat(40) })), false)
  assert.equal(isCandidatePreviewIdentity(identity({ candidateCommit: 'a'.repeat(39) })), false)
  assert.equal(isCandidatePreviewIdentity(identity({ candidateCommit: 'a'.repeat(65) })), false)
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

test('RUN-03 rapid start clicks share one in-flight request', async () => {
  let resolveStart
  const port = createPort({
    startRun() {
      port.calls.start += 1
      return new Promise(resolve => { resolveStart = resolve })
    },
  })
  const model = createCandidatePreviewViewModel({
    port,
    nextRequestId: () => 'req_1',
  })
  await model.start()
  const first = model.startRun()
  const second = model.startRun()
  await flush()
  assert.equal(port.calls.start, 1)
  assert.match(model.state.notice ?? '', /正在启动/)
  resolveStart(runControl({ phase: 'ready' }))
  await Promise.all([first, second])
  assert.equal(model.state.run?.phase, 'ready')
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

test('RUN-07 does not read a candidate file before authorization or after revocation', async () => {
  let reads = 0
  const port = createPort({
    async readFileContent() {
      reads += 1
      return {
        path: 'README.md',
        mediaType: 'text/plain',
        contentEncoding: 'utf-8',
        dataBase64: Buffer.from('const answer = 42').toString('base64'),
        offset: 0,
        returnedBytes: 17,
        totalBytes: 17,
        nextOffset: null,
      }
    },
  })
  const model = createCandidatePreviewViewModel({ port, nextRequestId: () => 'req_1' })
  await model.start()
  await model.openFile('README.md')
  assert.equal(reads, 0)
  await model.authorizePreview()
  await model.openFile('README.md')
  assert.equal(reads, 1)
  await model.revokePreview()
  await model.openFile('README.md')
  assert.equal(reads, 1)
})

test('RUN-07 preserves a backend degraded classification for a safe source path', async () => {
  const port = createPort({
    async listFiles() {
      return [{
        path: 'src/app.ts',
        previewClass: 'degraded',
        degradationReason: '该文件过大，当前仅可下载。',
        totalBytes: 12_345,
      }]
    },
  })
  const model = createCandidatePreviewViewModel({
    port,
    nextRequestId: () => 'req_1',
  })
  await model.start()
  await model.authorizePreview()
  assert.equal(model.state.files[0]?.previewClass, 'degraded')
  assert.equal(model.state.files[0]?.degradationReason, '该文件过大，当前仅可下载。')
  model.openFile('src/app.ts')
  assert.equal(model.state.navigation.path, '/')
  assert.equal(model.state.notice, '该文件过大，当前仅可下载。')
})

test('RUN-07 opens bounded text content and keeps HTML/SVG on the isolated preview route', async () => {
  const calls = []
  const port = createPort({
    async listFiles() {
      return [classifyPreviewFile('README.md'), classifyPreviewFile('public/index.html')]
    },
    async readFileContent(input) {
      calls.push(input.path)
      return {
        path: input.path,
        mediaType: 'text/markdown',
        contentEncoding: 'utf-8',
        dataBase64: Buffer.from('# Candidate').toString('base64'),
        offset: 0,
        returnedBytes: 11,
        totalBytes: 11,
        nextOffset: null,
      }
    },
  })
  const model = createCandidatePreviewViewModel({
    port,
    nextRequestId: () => 'req_1',
  })
  await model.start()
  await model.authorizePreview()
  await model.openFile('README.md')
  assert.equal(model.state.fileContent?.path, 'README.md')
  assert.deepEqual(calls, ['README.md'])
  await model.openFile('public/index.html')
  assert.deepEqual(calls, ['README.md'])
  assert.equal(model.state.navigation.path, '/public/index.html')
  assert.match(model.state.notice ?? '', /独立受控预览来源/u)
})

test('RUN-07 never renders a truncated image chunk as an image', async () => {
  const rootElement = new TrackedDocument().createElement('main')
  const port = createPort({
    async listFiles() {
      return [classifyPreviewFile('public/photo.png')]
    },
    async readFileContent(input) {
      return {
        path: input.path,
        mediaType: 'image/png',
        contentEncoding: 'binary',
        dataBase64: Buffer.from('partial').toString('base64'),
        offset: 0,
        returnedBytes: 7,
        totalBytes: 8,
        nextOffset: 7,
      }
    },
  })
  const model = createCandidatePreviewViewModel({ port, nextRequestId: () => 'req_1' })
  await model.start()
  await model.authorizePreview()
  await model.openFile('public/photo.png')
  const page = mountCandidateRunPreviewPage({ root: rootElement, model })
  await flush()
  const content = findByClass(rootElement, 'wwc-candidate-run-preview-file-content')
  assert.ok(content)
  assert.equal(findByClass(content, 'wwc-candidate-run-preview-file-content-image'), null)
  const status = findByClass(content, 'wwc-candidate-run-preview-file-content-status')
  assert.ok(status)
  assert.match(status.textContent, /二进制内容处理/u)
  page.close()
})

test('RUN-07 keeps unknown file types download-only even when bytes are UTF-8', async () => {
  const rootElement = new TrackedDocument().createElement('main')
  const port = createPort({
    async listFiles() {
      return [classifyPreviewFile('assets/data.txt')]
    },
    async readFileContent(input) {
      return {
        path: input.path,
        mediaType: 'text/plain',
        contentEncoding: 'utf-8',
        dataBase64: Buffer.from('secret text').toString('base64'),
        offset: 0,
        returnedBytes: 11,
        totalBytes: 11,
        nextOffset: null,
      }
    },
  })
  const model = createCandidatePreviewViewModel({ port, nextRequestId: () => 'req_1' })
  await model.start()
  await model.authorizePreview()
  await model.openFile('assets/data.txt')
  mountCandidateRunPreviewPage({ root: rootElement, model })
  await flush()
  const content = findByClass(rootElement, 'wwc-candidate-run-preview-file-content')
  assert.ok(content)
  assert.equal(findByClass(content, 'wwc-candidate-run-preview-file-content-code'), null)
  assert.ok(findByClass(content, 'wwc-candidate-run-preview-file-content-download'))
})

test('RUN-07 read failures use safe copy and keep retryability visible', async () => {
  const rootElement = new TrackedDocument().createElement('main')
  const port = createPort({
    async listFiles() {
      return [classifyPreviewFile('src/app.ts')]
    },
    async readFileContent() {
      throw new Error('/Users/alice/private/app.ts')
    },
    async readDiff() {
      throw { code: 'NETWORK_ERROR', kind: 'network', retryable: true, message: 'internal stack' }
    },
  })
  const model = createCandidatePreviewViewModel({ port, nextRequestId: () => 'req_1' })
  await model.start()
  await model.authorizePreview()
  await model.openFile('src/app.ts')
  assert.equal(model.state.fileContent?.error, '文件内容读取失败，请重试。')
  assert.equal(model.state.fileContent?.retryable, true)
  mountCandidateRunPreviewPage({ root: rootElement, model })
  const fileStatus = findByClass(rootElement, 'wwc-candidate-run-preview-file-content-status')
  assert.match(fileStatus.textContent, /请重试/u)
  assert.equal(fileStatus.getAttribute('role'), 'alert')
  assert.doesNotMatch(fileStatus.textContent, /Users\/alice|internal stack/u)
  const fileRetry = findByClass(rootElement, 'wwc-candidate-run-preview-file-content-retry')
  assert.equal(fileRetry.textContent, '重试读取文件')
  assert.equal(findByClass(rootElement, 'wwc-candidate-run-preview-file-content').tabIndex, -1)
  fileRetry.emit('click')
  await flush()

  await model.openDiff('src/app.ts')
  assert.equal(model.state.fileDiff?.error, '变更 diff读取失败，请重试。')
  assert.equal(model.state.fileDiff?.retryable, true)
  const diffStatus = findByClass(rootElement, 'wwc-candidate-run-preview-diff-status')
  assert.match(diffStatus.textContent, /请重试/u)
  assert.equal(diffStatus.getAttribute('role'), 'alert')
  assert.doesNotMatch(diffStatus.textContent, /internal stack/u)
  const diffRetry = findByClass(rootElement, 'wwc-candidate-run-preview-diff-retry')
  assert.equal(diffRetry.textContent, '重试读取 diff')
  assert.equal(findByClass(rootElement, 'wwc-candidate-run-preview-diff').tabIndex, -1)
})

test('RUN-07 reads the canonical candidate file pages and rejects stale identity', async () => {
  const deliveryId = 'dlv_00000000000000000000000042'
  const candidateRef = `git-candidate:sha256:${'c'.repeat(64)}`
  const candidateTreeId = 'b'.repeat(40)
  const diffSha256 = `sha256:${'d'.repeat(64)}`
  const scope = {
    kind: 'repository',
    organizationId: 'org_00000000000000000000000001',
    workspaceId: 'wsp_00000000000000000000000001',
    projectId: 'prj_00000000000000000000000001',
    repositoryId: 'rep_00000000000000000000000001',
  }
  const cursor = {
    token: `sfread_${'1'.repeat(32)}`,
    scope,
    deliveryId,
    deliveryRevision: 2,
    runtimeLedgerRevision: 1,
    runtimeAcceptedSequence: 1,
    publicationRevision: 0,
    eventCursor: {
      scope,
      stream: { kind: 'delivery', deliveryId },
      sequence: 1,
      eventId: 'evt_00000000000000000000000042',
    },
  }
  const requests = []
  const candidate = {
    candidateCommitId: 'a'.repeat(40),
    candidateRef,
    candidateTreeId,
    deliverySpecId: 'spec_1',
    deliverySpecRevision: 1,
    diffSha256,
    frozenAt: '2026-09-12T00:00:00.000Z',
    producerSessionBindingId: 'binding_1',
    producerWorkRunId: 'wrn_00000000000000000000000042',
  }
  const port = createControlPlaneCandidatePreviewPort({
    serverUrl: 'https://server.example.test',
    identity: identity({ candidateTreeId }),
    client: {
      async query(request) {
        requests.push(request)
        if (request.query === 'candidate.diff.get') {
          const text = '@@ -1 +1 @@\n-old\n+new\n'
          return {
            query: request.query,
            result: {
              kind: 'candidate_diff_chunk',
              candidate,
              readCursor: cursor,
              path: request.parameters.path,
              oldPath: null,
              status: 'modified',
              binary: false,
              contentEncoding: 'utf-8',
              encoding: 'base64',
              dataBase64: Buffer.from(text).toString('base64'),
              mediaType: 'application/vnd.winwincode.git-diff',
              fileDiffSha256: diffSha256,
              offset: request.parameters.offset,
              returnedBytes: Buffer.byteLength(text),
              totalBytes: Buffer.byteLength(text),
              nextOffset: null,
            },
            page: { hasMore: false, nextCursor: null },
          }
        }
        if (request.query === 'candidate.file.content.get') {
          const text = 'const answer = 42\n'
          return {
            query: request.query,
            result: {
              kind: 'candidate_file_content_chunk',
              candidate,
              readCursor: cursor,
              path: request.parameters.path,
              contentEncoding: 'utf-8',
              encoding: 'base64',
              mediaType: 'text/plain',
              dataBase64: Buffer.from(text).toString('base64'),
              offset: request.parameters.offset,
              returnedBytes: Buffer.byteLength(text),
              totalBytes: Buffer.byteLength(text),
              nextOffset: null,
            },
            page: { hasMore: false, nextCursor: null },
          }
        }
        return {
          query: 'candidate.files.list',
          result: {
            kind: 'candidate_file_page',
            candidate,
            readCursor: cursor,
            items: [
              { path: 'src/app.ts', oldPath: null, status: 'modified', additions: 1, deletions: 0, binary: false, encoding: 'utf-8' },
              { path: 'src/removed.ts', oldPath: null, status: 'deleted', additions: 0, deletions: 1, binary: false, encoding: 'utf-8' },
              { path: 'assets/logo.png', oldPath: null, status: 'added', additions: null, deletions: null, binary: true, encoding: 'binary' },
              { path: 'data/legacy.txt', oldPath: null, status: 'added', additions: null, deletions: null, binary: false, encoding: 'unknown-8bit' },
            ].filter(() => requests.length === 1),
          },
          page: requests.length === 1
            ? { hasMore: true, nextCursor: 'page_2' }
            : { hasMore: false, nextCursor: null },
        }
      },
    },
    actor: { kind: 'user', id: 'usr_00000000000000000000000001' },
    scope,
    fileContext: { deliveryId, atCursor: cursor, candidateRef, candidateTreeId, diffSha256 },
    nextRequestId: () => 'req_00000000000000000000000042',
  })
  const files = await port.listFiles({ sourceId: 'pvs_demo' })
  assert.deepEqual(files.map(file => file.previewClass), ['code', 'degraded', 'image', 'download'])
  assert.match(files[1].degradationReason, /删除/u)
  assert.equal(files[2].degradationReason, null)
  assert.match(files[3].degradationReason, /仅提供下载/u)
  assert.equal(requests[0].query, 'candidate.files.list')
  assert.equal(requests[1].page.cursor, 'page_2')
  assert.deepEqual(requests[0].parameters, {
    atCursor: cursor,
    candidateRef,
    candidateTreeId,
    deliveryId,
    diffSha256,
    pathPrefix: null,
    readPageLimit: 100,
    statuses: [],
  })
  const diff = await port.readDiff({ sourceId: 'pvs_demo', path: 'src/app.ts', offset: 0 })
  assert.equal(diff.text, '@@ -1 +1 @@\n-old\n+new\n')
  assert.equal(requests[2].query, 'candidate.diff.get')
  const content = await port.readFileContent({ sourceId: 'pvs_demo', path: 'src/app.ts', offset: 0 })
  assert.equal(content.dataBase64, Buffer.from('const answer = 42\n').toString('base64'))
  assert.equal(requests[3].query, 'candidate.file.content.get')
  assert.equal(requests[2].parameters.length, 64 * 1024)

  const stalePort = createControlPlaneCandidatePreviewPort({
    serverUrl: 'https://server.example.test',
    identity: identity({ candidateTreeId }),
    client: {
      async query() {
        return {
          query: 'candidate.files.list',
          result: { kind: 'candidate_file_page', candidate: { ...candidate, candidateTreeId: 'e'.repeat(40) }, readCursor: cursor, items: [] },
          page: { hasMore: false, nextCursor: null },
        }
      },
    },
    actor: { kind: 'user', id: 'usr_00000000000000000000000001' },
    scope,
    fileContext: { deliveryId, atCursor: cursor, candidateRef, candidateTreeId, diffSha256 },
    nextRequestId: () => 'req_00000000000000000000000042',
  })
  await assert.rejects(
    stalePort.listFiles({ sourceId: 'pvs_demo' }),
    error => error.code === 'STALE_CANDIDATE_FILE_PAGE',
  )
})

test('RUN-07 rejects a UTF-8 file chunk with invalid byte sequences', async () => {
  const context = {
    deliveryId: 'dlv_00000000000000000000000042',
    candidateRef: `git-candidate:sha256:${'c'.repeat(64)}`,
    candidateTreeId: 'b'.repeat(40),
    diffSha256: `sha256:${'d'.repeat(64)}`,
  }
  const scope = {
    kind: 'repository',
    organizationId: 'org_00000000000000000000000001',
    workspaceId: 'wsp_00000000000000000000000001',
    projectId: 'prj_00000000000000000000000001',
    repositoryId: 'rep_00000000000000000000000001',
  }
  const cursor = {
    token: `sfread_${'1'.repeat(32)}`,
    scope,
    deliveryId: context.deliveryId,
    deliveryRevision: 2,
    runtimeLedgerRevision: 1,
    runtimeAcceptedSequence: 1,
    publicationRevision: 0,
    eventCursor: {
      scope,
      stream: { kind: 'delivery', deliveryId: context.deliveryId },
      sequence: 1,
      eventId: 'evt_00000000000000000000000042',
    },
  }
  const candidate = {
    candidateCommitId: 'a'.repeat(40),
    candidateRef: context.candidateRef,
    candidateTreeId: context.candidateTreeId,
    deliverySpecId: 'spec_1',
    deliverySpecRevision: 1,
    diffSha256: context.diffSha256,
    frozenAt: '2026-09-12T00:00:00.000Z',
    producerSessionBindingId: 'binding_1',
    producerWorkRunId: 'wrn_00000000000000000000000042',
  }
  const port = createControlPlaneCandidatePreviewPort({
    serverUrl: 'https://server.example.test',
    identity: identity({ candidateTreeId: context.candidateTreeId }),
    client: {
      async query(request) {
        return {
          query: request.query,
          result: {
            kind: 'candidate_file_content_chunk',
            candidate,
            readCursor: cursor,
            path: request.parameters.path,
            contentEncoding: 'utf-8',
            encoding: 'base64',
            mediaType: 'text/plain',
            dataBase64: '//4=',
            offset: request.parameters.offset,
            returnedBytes: 2,
            totalBytes: 2,
            nextOffset: null,
          },
          page: { hasMore: false, nextCursor: null },
        }
      },
    },
    actor: { kind: 'user', id: 'usr_00000000000000000000000001' },
    scope,
    fileContext: { ...context, atCursor: cursor },
    nextRequestId: () => 'req_00000000000000000000000042',
  })
  await assert.rejects(
    port.readFileContent({ sourceId: 'pvs_demo', path: 'src/app.ts', offset: 0 }),
    error => error.code === 'INVALID_CANDIDATE_FILE_CONTENT',
  )
})

test('RUN-07 gives degraded files a safe fallback reason', async () => {
  const port = createPort({
    async listFiles() {
      return [{ path: 'src/app.ts', previewClass: 'degraded', degradationReason: null, totalBytes: null }]
    },
  })
  const model = createCandidatePreviewViewModel({
    port,
    nextRequestId: () => 'req_1',
  })
  await model.start()
  await model.authorizePreview()
  model.openFile('src/app.ts')
  assert.equal(model.state.notice, '该文件暂不可在预览中打开。')
})

test('RUN-07 opens a bounded code diff with continuation', async () => {
  const reads = []
  const port = createPort({
    async listFiles() {
      return [classifyPreviewFile('src/app.ts')]
    },
    async readDiff(input) {
      reads.push(input)
      return {
        path: input.path,
        oldPath: null,
        status: 'modified',
        text: input.offset === 0 ? '@@ -1 +1 @@\n-old\n' : '+new\n',
        offset: input.offset,
        returnedBytes: input.offset === 0 ? 16 : 5,
        totalBytes: 21,
        nextOffset: input.offset === 0 ? 16 : null,
      }
    },
  })
  const model = createCandidatePreviewViewModel({ port, nextRequestId: () => 'req_1' })
  await model.start()
  await model.authorizePreview()
  await model.openDiff('src/app.ts')
  assert.equal(model.state.fileDiff?.text, '@@ -1 +1 @@\n-old\n')
  assert.equal(model.state.fileDiff?.nextOffset, 16)
  await model.continueDiff()
  assert.equal(model.state.fileDiff?.text, '@@ -1 +1 @@\n-old\n+new\n')
  assert.deepEqual(reads, [
    { sourceId: 'pvs_demo', path: 'src/app.ts', offset: 0 },
    { sourceId: 'pvs_demo', path: 'src/app.ts', offset: 16 },
  ])
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

test('RUN-08 production port reads only canonical, bounded UTF-8 Evidence logs', async () => {
  const scope = {
    kind: 'repository',
    organizationId: 'org_00000000000000000000000001',
    workspaceId: 'wsp_00000000000000000000000001',
    projectId: 'prj_00000000000000000000000001',
    repositoryId: 'rep_00000000000000000000000001',
  }
  const deliveryId = 'dlv_00000000000000000000000001'
  const workRunId = 'wrn_00000000000000000000000001'
  const evidenceId = 'evd_00000000000000000000000001'
  const artifactId = 'art_00000000000000000000000001'
  const candidateRef = `git-candidate:sha256:${'a'.repeat(64)}`
  const cursor = {
    token: `sfread_${'1'.repeat(32)}`,
    scope,
    deliveryId,
    deliveryRevision: 1,
    runtimeLedgerRevision: 1,
    runtimeAcceptedSequence: 1,
    publicationRevision: 0,
    eventCursor: {
      scope,
      stream: { kind: 'delivery', deliveryId },
      sequence: 1,
      eventId: 'evt_00000000000000000000000001',
    },
  }
  const evidence = {
    candidateRef,
    createdAt: '2026-09-12T00:00:00.000Z',
    deliverySpecId: 'spec:current',
    deliverySpecRevision: 1,
    id: evidenceId,
    sessionBindingId: 'binding:reviewer',
    sourceRef: 'runtime://work-run/log',
    type: 'runtime_event',
    workRunId,
  }
  const text = `${Array.from({ length: PREVIEW_LOG_MAX_LINES + 3 }, (_, index) => `line-${index + 1}`).join('\n')}\n`
  const descriptor = {
    artifactId,
    digest: `sha256:${'b'.repeat(64)}`,
    fileName: 'stdout.log',
    kind: 'log',
    mediaType: 'text/plain',
    previewMode: 'inline_text',
    provenance: {
      candidateRef,
      deliveryId,
      deliveryRevision: 1,
      evidenceId,
      sessionBindingId: evidence.sessionBindingId,
      workRunId,
    },
    sizeBytes: Buffer.byteLength(text),
  }
  const projection = {
    deliveryId,
    readCursor: cursor,
    workRun: { id: workRunId },
    evidence: [evidence],
  }
  const client = {
    async query(request) {
      if (request.query === 'evidence.get') {
        return {
          query: request.query,
          result: {
            artifactAccess: { state: 'available', items: [descriptor] },
            evidence,
            kind: 'evidence_detail',
            outcome: 'observed',
            readCursor: cursor,
          },
          page: { hasMore: false, nextCursor: null },
        }
      }
      return {
        query: request.query,
        result: {
          artifact: descriptor,
          contentEncoding: 'utf-8',
          dataBase64: Buffer.from(text).toString('base64'),
          encoding: 'base64',
          evidence,
          kind: 'evidence_artifact_content_chunk',
          nextOffset: null,
          offset: 0,
          previewMode: 'inline_text',
          readCursor: cursor,
          returnedBytes: Buffer.byteLength(text),
          totalBytes: Buffer.byteLength(text),
          truncated: true,
          state: 'available',
        },
        page: { hasMore: false, nextCursor: null },
      }
    },
  }
  const port = createControlPlaneCandidatePreviewPort({
    serverUrl: 'https://server.example.test',
    identity: identity(),
    client,
    actor: { id: 'usr_00000000000000000000000001', kind: 'user' },
    scope,
    runIdentityProjection: async () => projection,
    nextRequestId: () => 'req_00000000000000000000000001',
  })
  const segments = await port.listLogSegments({ runId: workRunId })
  assert.equal(segments.length, 1)
  assert.equal(segments[0].lineCount, PREVIEW_LOG_MAX_LINES)
  assert.equal(segments[0].maxLines, PREVIEW_LOG_MAX_LINES)
  assert.equal(segments[0].truncated, true)
  const citation = await port.readLogCitation({
    runId: workRunId,
    segmentKey: segments[0].key,
    lineStart: 1,
    lineEnd: 2,
  })
  assert.equal(citation?.redactedText, 'line-1\nline-2')
  assert.equal(citation?.sourceRef, evidence.sourceRef)

  const invalidEncoding = {
    ...client,
    async query(request) {
      const response = await client.query(request)
      if (request.query === 'evidence.artifact.content.get') response.result.contentEncoding = 'unknown-8bit'
      return response
    },
  }
  const invalidPort = createControlPlaneCandidatePreviewPort({
    serverUrl: 'https://server.example.test',
    identity: identity(),
    client: invalidEncoding,
    actor: { id: 'usr_00000000000000000000000001', kind: 'user' },
    scope,
    runIdentityProjection: async () => projection,
    nextRequestId: () => 'req_00000000000000000000000001',
  })
  assert.deepEqual(await invalidPort.listLogSegments({ runId: workRunId }), [])

  const unavailable = {
    ...client,
    async query(request) {
      const response = await client.query(request)
      if (request.query === 'evidence.artifact.content.get') {
        response.result.artifact.provenance.workRunId = 'wrn_00000000000000000000000002'
      }
      return response
    },
  }
  const rejectedPort = createControlPlaneCandidatePreviewPort({
    serverUrl: 'https://server.example.test',
    identity: identity(),
    client: unavailable,
    actor: { id: 'usr_00000000000000000000000001', kind: 'user' },
    scope,
    runIdentityProjection: async () => projection,
    nextRequestId: () => 'req_00000000000000000000000001',
  })
  assert.deepEqual(await rejectedPort.listLogSegments({ runId: workRunId }), [])
})

test('RUN-03 keeps managed-app source logs separate from candidate producer evidence', async () => {
  const producerWorkRunId = 'wrn_00000000000000000000000002'
  const sourceWorkRunId = 'wrn_00000000000000000000000003'
  const deliveryId = 'dlv_00000000000000000000000001'
  const evidence = {
    candidateRef: `git-candidate:sha256:${'a'.repeat(64)}`,
    createdAt: '2026-09-12T00:00:00.000Z',
    deliverySpecId: 'spec:current',
    deliverySpecRevision: 1,
    id: 'evd_00000000000000000000000003',
    sessionBindingId: 'binding:source',
    sourceRef: 'runtime://source/log',
    type: 'runtime_event',
    workRunId: sourceWorkRunId,
  }
  const producerEvidence = { ...evidence, id: 'evd_00000000000000000000000002', workRunId: producerWorkRunId, sourceRef: 'runtime://producer/log' }
  const scope = {
    kind: 'repository',
    organizationId: 'org_00000000000000000000000001',
    workspaceId: 'wsp_00000000000000000000000001',
    projectId: 'prj_00000000000000000000000001',
    repositoryId: 'rep_00000000000000000000000001',
  }
  const cursor = {
    token: `sfread_${'1'.repeat(32)}`,
    scope,
    deliveryId,
    deliveryRevision: 1,
    runtimeLedgerRevision: 1,
    runtimeAcceptedSequence: 1,
    publicationRevision: 0,
    eventCursor: {
      scope,
      stream: { kind: 'delivery', deliveryId },
      sequence: 1,
      eventId: 'evt_00000000000000000000000001',
    },
  }
  const descriptor = {
    artifactId: 'art_00000000000000000000000003',
    digest: `sha256:${'b'.repeat(64)}`,
    fileName: 'stdout.log',
    kind: 'log',
    mediaType: 'text/plain',
    previewMode: 'inline_text',
    provenance: {
      candidateRef: evidence.candidateRef,
      deliveryId,
      deliveryRevision: 1,
      evidenceId: evidence.id,
      sessionBindingId: evidence.sessionBindingId,
      workRunId: sourceWorkRunId,
    },
    sizeBytes: 6,
  }
  const requests = []
  const projection = {
    deliveryId,
    readCursor: cursor,
    workRun: { id: producerWorkRunId },
    managedAppSourceRun: { id: sourceWorkRunId },
    evidence: [producerEvidence],
    managedAppSourceEvidence: [evidence],
  }
  const client = {
    async query(request) {
      requests.push(request)
      if (request.query === 'evidence.get') {
        return {
          query: request.query,
          result: {
            artifactAccess: { state: 'available', items: [descriptor] },
            evidence,
            kind: 'evidence_detail',
            outcome: 'observed',
            readCursor: cursor,
          },
          page: { hasMore: false, nextCursor: null },
        }
      }
      return {
        query: request.query,
        result: {
          artifact: descriptor,
          contentEncoding: 'utf-8',
          dataBase64: Buffer.from('ready\n').toString('base64'),
          encoding: 'base64',
          evidence,
          kind: 'evidence_artifact_content_chunk',
          nextOffset: null,
          offset: 0,
          previewMode: 'inline_text',
          readCursor: cursor,
          returnedBytes: 6,
          totalBytes: 6,
          truncated: false,
          state: 'available',
        },
        page: { hasMore: false, nextCursor: null },
      }
    },
  }
  const port = createControlPlaneCandidatePreviewPort({
    serverUrl: 'https://server.example.test',
    identity: identity({ workRunId: sourceWorkRunId }),
    client,
    actor: { id: 'usr_00000000000000000000000001', kind: 'user' },
    scope,
    runIdentityProjection: async () => projection,
    nextRequestId: () => 'req_00000000000000000000000001',
  })
  const segments = await port.listLogSegments({ runId: sourceWorkRunId })
  assert.equal(segments.length, 1)
  assert.equal(requests[0].parameters.workRunId, sourceWorkRunId)
  assert.equal(requests[0].parameters.evidenceId, evidence.id)
  assert.deepEqual(await port.listLogSegments({ runId: producerWorkRunId }), [])
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
  const identityLine = findByClass(rootElement, 'wwc-candidate-run-preview-identity')
  assert.equal(identityLine.textContent, '当前任务 · 第 1 次运行 · 版本 aaaaaaa')
  assert.doesNotMatch(identityLine.textContent, /pvs_demo|cfg-1|tree/u)
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

  const file = findByClass(rootElement, 'wwc-candidate-run-preview-file-open')
  assert.ok(file)
  file.emit('click')
  await flush()
  const fileContent = findByClass(rootElement, 'wwc-candidate-run-preview-file-content')
  assert.ok(fileContent)
  assert.ok(findByClass(fileContent, 'wwc-candidate-run-preview-file-content-code'))
  assert.equal(model.state.navigation.path, '/')

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
        previewUrl: `https://preview.example.test/p/pva_0123456789abcdef0123456789abcdef/${'b'.repeat(64)}/`,
        expiresAt: '2026-09-12T00:10:00.000Z',
        source: {
          sourceId: 'pvs_demo',
          workRunId: 'wrn_demo',
          repositoryBindingId: 'rb_demo',
          mode: 'frozen-candidate',
          candidateCommit: 'a'.repeat(40),
        },
      }), { status: 201 })
    },
  })

  const granted = await port.authorizePreview({ sourceId: 'pvs_demo', requestId: 'req_1' })
  assert.equal(granted.previewAccessId, 'pva_0123456789abcdef0123456789abcdef')
  assert.equal(port.listFiles, undefined)
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
          workRunId: 'wrn_demo',
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

test('RUN-05 browser port rejects a preview grant on the management origin', async () => {
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
          sourceId: 'pvs_demo',
          workRunId: 'wrn_demo',
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

test('RUN-05 browser port preserves the Server preview error envelope', async () => {
  const port = createControlPlaneCandidatePreviewPort({
    serverUrl: 'https://server.example.test',
    identity: identity(),
    async fetch() {
      return new Response(JSON.stringify({
        schemaVersion: 'winwincode/v1',
        requestId: 'req_00000000000000000000000042',
        error: {
          code: 'WRONG_STATE',
          message: 'preview source is not connected',
          retryable: false,
          details: { reason: 'source_offline' },
        },
      }), { status: 409 })
    },
  })

  await assert.rejects(
    port.authorizePreview({ sourceId: 'pvs_demo', requestId: 'req_1' }),
    error => error.code === 'WRONG_STATE'
      && error.message === 'preview source is not connected'
      && error.requestId === 'req_00000000000000000000000042'
      && error.details.reason === 'source_offline'
      && error.retryable === false,
  )
})

test('RUN-05 browser port defaults a real Server outage to retryable', async () => {
  const port = createControlPlaneCandidatePreviewPort({
    serverUrl: 'https://server.example.test',
    identity: identity(),
    async fetch() {
      return new Response(JSON.stringify({
        schemaVersion: 'winwincode/v1',
        error: { code: 'PREVIEW_UNAVAILABLE', message: 'preview service is unavailable' },
      }), { status: 502 })
    },
  })

  await assert.rejects(
    port.authorizePreview({ sourceId: 'pvs_demo', requestId: 'req_1' }),
    error => error.code === 'PREVIEW_UNAVAILABLE' && error.retryable === true,
  )
})

test('RUN-05 browser port classifies too many requests as a Server error', async () => {
  const port = createControlPlaneCandidatePreviewPort({
    serverUrl: 'https://server.example.test',
    identity: identity(),
    async fetch() {
      return new Response(JSON.stringify({
        schemaVersion: 'winwincode/v1',
        error: { code: 'TOO_MANY_REQUESTS', message: 'slow down', retryable: true },
      }), { status: 429 })
    },
  })

  await assert.rejects(
    port.authorizePreview({ sourceId: 'pvs_demo', requestId: 'req_1' }),
    error => error.kind === 'server'
      && error.code === 'TOO_MANY_REQUESTS'
      && error.retryable === true,
  )
})

test('RUN-05 browser port maps access errors from the Server envelope', async () => {
  const port = createControlPlaneCandidatePreviewPort({
    serverUrl: 'https://server.example.test',
    identity: identity(),
    async fetch() {
      return new Response(JSON.stringify({
        schemaVersion: 'winwincode/v1',
        error: { code: 'AUTHENTICATION_REQUIRED', message: 'sign in required', retryable: false },
      }), { status: 401 })
    },
  })

  await assert.rejects(
    port.authorizePreview({ sourceId: 'pvs_demo', requestId: 'req_1' }),
    error => error.kind === 'authentication' && error.code === 'AUTHENTICATION_REQUIRED',
  )
})

test('RUN-03 managed-app controls use the current occupancy lease and candidate config', async () => {
  const requests = []
  const config = {
    schemaVersion: 'winwincode/managed-app-run-v1',
    runId: 'wrn_demo',
    attempt: 1,
    mode: 'frozen-candidate',
    candidateCommit: 'a'.repeat(40),
  }
  const port = createControlPlaneCandidatePreviewPort({
    serverUrl: 'https://server.example.test',
    identity: identity(),
    managedAppRunConfig: config,
    async occupancyStatus() {
      return {
        occupancy: 'occupied',
        presence: 'online',
        holderUserId: 'usr_owner',
        occupancyLeaseId: 'ocl_demo',
        fencingToken: 7,
        claimedAt: null,
        acknowledgedAt: null,
        recoveryDeadlineAt: null,
        capacityUsed: 0,
        capacityTotal: 1,
      }
    },
    async fetch(_url, init) {
      requests.push(JSON.parse(init.body))
      return new Response(JSON.stringify({
        schemaVersion: 'winwincode/managed-app-run-v1',
        runId: 'wrn_demo',
        leaseId: 'ocl_demo',
        phase: requests.at(-1).operation === 'stop' ? 'exited' : 'starting',
        startedAt: null,
        exitedAt: null,
        exitCode: null,
        failureReason: null,
      }), { status: 202 })
    },
  })

  assert.equal(typeof port.startRun, 'function')
  const started = await port.startRun({ identity: identity(), requestId: 'req_start' })
  assert.equal(started.phase, 'starting')
  assert.equal(requests[0].operation, 'start')
  assert.equal(requests[0].occupancyLeaseId, 'ocl_demo')
  assert.equal(requests[0].occupancyFencingToken, 7)
  assert.equal(requests[0].config, null)
  assert.equal(requests[0].runId, 'wrn_demo')

  const stopped = await port.stopRun({ runId: 'wrn_demo', leaseId: 'ocl_demo', requestId: 'req_stop' })
  assert.equal(stopped.phase, 'exited')
  assert.equal(requests[1].operation, 'stop')
  assert.equal(requests[1].config, null)
})

test('RUN-03 managed-app controls stay unavailable without a real run config', () => {
  const port = createControlPlaneCandidatePreviewPort({
    serverUrl: 'https://server.example.test',
    identity: identity(),
    async occupancyStatus() {
      throw new Error('must not be called while config is absent')
    },
  })
  assert.equal(port.startRun, undefined)
  assert.equal(port.stopRun, undefined)
  assert.equal(port.restartRun, undefined)
  assert.equal(port.queryRun, undefined)
})
