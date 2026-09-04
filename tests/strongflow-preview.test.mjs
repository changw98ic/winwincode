import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
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
    'apps/client/tsconfig.strongflow-preview-tests.json',
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
  `StrongFlow Preview did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const previewModule = await import(`${pathToFileURL(resolve(
  root,
  '.cache/strongflow-preview-tests/strongflow-preview.js',
)).href}`)
const evidenceModule = await import(`${pathToFileURL(resolve(
  root,
  '.cache/strongflow-preview-tests/strongflow-evidence.js',
)).href}`)
const contracts = await import(`${pathToFileURL(resolve(
  root,
  '.cache/strongflow-preview-tests/generated/contracts.js',
)).href}`)

const {
  DEFAULT_PREVIEW_ROW_LIMIT,
  MAX_PREVIEW_IMAGE_BYTES,
  STRONGFLOW_PREVIEW_VALIDITY_MILLIS,
  strongFlowPreviewChannel,
  strongFlowPreviewImageSupport,
  strongFlowPreviewRowsForTab,
  strongFlowPreviewScreenshotNote,
  strongFlowPreviewSnapshot,
} = previewModule
const { createStrongFlowEvidenceViewModel, mountStrongFlowEvidence } = evidenceModule
const { EvidenceOutcome, QueryName } = contracts

const schemaVersion = 'winwincode/v1'
const deliveryId = 'dlv_00000000000000000000000001'
const stageRunId = 'run_00000000000000000000000001'
const sessionBindingId = 'sbd_00000000000000000000000001'
const actor = { kind: 'user', id: 'usr_00000000000000000000000001' }
const scope = {
  kind: 'repository',
  organizationId: 'org_00000000000000000000000001',
  workspaceId: 'wsp_00000000000000000000000001',
  projectId: 'prj_00000000000000000000000001',
  repositoryId: 'rep_00000000000000000000000001',
}
const currentCandidateRef = 'git-candidate:sha256:' + 'a'.repeat(64)
const supersededCandidateRef = 'git-candidate:sha256:' + 'b'.repeat(64)
const snapshotUpdatedAt = '2026-09-02T12:00:00.000Z'

const PNG_BYTES = Object.freeze([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a])

function evidenceId(value) {
  return `evd_${String(value).padStart(26, '0')}`
}

function readCursor(overrides = {}) {
  return {
    deliveryId,
    deliveryRevision: 6,
    eventCursor: {
      eventId: 'evt_00000000000000000000000001',
      sequence: 12,
      scope,
      stream: { kind: 'delivery', deliveryId },
    },
    publicationRevision: 0,
    runtimeAcceptedSequence: 4,
    runtimeLedgerRevision: 5,
    scope,
    token: 'cursor-token-1',
    ...overrides,
  }
}

function evidenceRow(value, overrides = {}) {
  return {
    candidateRef: currentCandidateRef,
    createdAt: '2026-09-02T10:00:00.000Z',
    deliverySpecId: 'spec:1',
    deliverySpecRevision: 2,
    id: evidenceId(value),
    sessionBindingId,
    sourceRef: `runtime_event:${String(value).padStart(26, '0')}`,
    stageRunId,
    type: 'runtime_event',
    ...overrides,
  }
}

/** The exact row the reads below answer with, so identity checks stay exact. */
const browserRow2 = evidenceRow(2, { createdAt: '2026-09-02T11:00:00.000Z' })

function browserProjection(overrides = {}) {
  return {
    delivery: {
      schemaVersion,
      deliveryId,
      deliveryRevision: 6,
      status: 'executing',
      ownership: {
        organizationId: scope.organizationId,
        workspaceId: scope.workspaceId,
        projectId: scope.projectId,
        repositoryId: scope.repositoryId,
      },
      requirements: {
        title: 'Preview workbench',
        goal: 'Inspect Candidate screenshots and Preview health safely.',
      },
      tasks: [],
      stages: [{
        id: stageRunId,
        stage: 'executing',
        role: 'implementer',
        status: 'running',
      }],
      attention: [],
    },
    solutionReview: null,
    stage: { id: stageRunId },
    runtime: { stageRunId, sessions: [] },
    evidence: overrides.evidence ?? [
      evidenceRow(1, { type: 'test', sourceRef: 'test:browser:1' }),
      browserRow2,
      evidenceRow(3, { type: 'command', sourceRef: 'command:build:1' }),
      evidenceRow(4, {
        type: 'test',
        sourceRef: 'test:superseded:1',
        candidateRef: supersededCandidateRef,
      }),
      evidenceRow(5, { type: 'diff', sourceRef: 'git_diff:sha256:' + 'c'.repeat(64) }),
    ],
    verdict: overrides.verdict === undefined ? null : overrides.verdict,
    attention: [],
    currentCandidate: 'currentCandidate' in overrides ? overrides.currentCandidate : {
      candidateRef: currentCandidateRef,
      candidateCommitId: '1'.repeat(40),
      candidateTreeId: '2'.repeat(40),
      diffSha256: `sha256:${'3'.repeat(64)}`,
      frozenAt: '2026-09-02T00:59:00.000Z',
    },
    publication: null,
    metadata: {
      source: 'control-plane-snapshot',
      updatedAt: snapshotUpdatedAt,
      revisions: { delivery: 6, deliverySpec: 2, runtime: 5, publication: 0 },
      readCursor: readCursor(),
    },
  }
}

function criterion(criterionId, verdict, evidenceRefs) {
  return {
    criterionId,
    evaluatedAt: '2026-09-02T11:30:00.000Z',
    evidenceRefs,
    explanation: `${criterionId} ${verdict}`,
    resultId: `result:${criterionId}`,
    verdict,
  }
}

function passingVerdict() {
  return {
    status: 'pass',
    producedAt: snapshotUpdatedAt,
    criteria: [criterion('criterion:browser', 'pass', [evidenceId(1)])],
  }
}

function modelState(overrides = {}) {
  return {
    status: 'ready',
    realtime: 'subscribed',
    projection: browserProjection(),
    interaction: { status: 'idle', error: null },
    error: null,
    ...overrides,
  }
}

class FakeStrongFlowModel {
  constructor(initialState = modelState()) {
    this.state = initialState
  }

  listener = null

  subscribe(listener) {
    this.listener = listener
    listener(this.state)
    return () => { this.listener = null }
  }

  publish(next) {
    this.state = next
    this.listener?.(next)
  }
}

class FakeControlPlaneClient {
  constructor(handler = () => null) {
    this.handler = handler
    this.queries = []
  }

  queryOptions = []

  async query(request, options) {
    this.queries.push(structuredClone(request))
    this.queryOptions.push(options ?? null)
    const response = this.handler(request, this.queries.length)
    if (response === null) throw new Error('unexpected query')
    return response
  }
}

let requestSequence = 0
function nextRequestId() {
  requestSequence += 1
  return `req_${String(requestSequence).padStart(26, '0')}`
}

function deepLink() {
  const state = { hash: '#/strongflow?delivery=' + deliveryId, replaced: [] }
  const link = {
    get route() {
      const parameters = new URLSearchParams(state.hash.slice(state.hash.indexOf('?') + 1))
      const tab = parameters.get('tab')
      return {
        tab: tab === 'preview' || tab === 'tests' || tab === 'logs' ? tab : 'evidence',
        evidenceId: parameters.get('evidence'),
      }
    },
    onRouteChange: route => {
      const parameters = new URLSearchParams(state.hash.slice(state.hash.indexOf('?') + 1))
      parameters.set('tab', route.tab)
      if (route.evidenceId === null) parameters.delete('evidence')
      else parameters.set('evidence', route.evidenceId)
      state.hash = `#/strongflow?${parameters.toString()}`
      state.replaced.push(state.hash)
    },
    state,
  }
  return link
}

function evidenceGet(request, result) {
  return {
    schemaVersion,
    requestId: request.requestId,
    query: QueryName.EvidenceGet,
    result,
    page: { hasMore: false, nextCursor: null },
  }
}

function evidenceDetailResult(overrides = {}) {
  return {
    kind: 'evidence_detail',
    artifactAccess: { state: 'unavailable', reason: 'no_authoritative_link' },
    evidence: browserRow2,
    outcome: EvidenceOutcome.Observed,
    readCursor: readCursor(),
    ...overrides,
  }
}

function descriptor(overrides = {}) {
  return {
    artifactId: 'art_0000000000000000000000001A',
    digest: `sha256:${'c'.repeat(64)}`,
    fileName: 'candidate.png',
    kind: 'report',
    mediaType: 'image/png',
    previewMode: 'inline_text',
    provenance: {
      candidateRef: currentCandidateRef,
      deliveryId,
      deliveryRevision: 6,
      evidenceId: evidenceId(2),
      sessionBindingId,
      stageRunId,
    },
    sizeBytes: PNG_BYTES.length,
    ...overrides,
  }
}

function chunkResponse(request, overrides = {}) {
  const artifact = overrides.artifact ?? descriptor()
  return {
    schemaVersion,
    requestId: request.requestId,
    query: QueryName.EvidenceArtifactContentGet,
    result: {
      artifact,
      contentEncoding: 'binary',
      dataBase64: Buffer.from(PNG_BYTES).toString('base64'),
      encoding: 'base64',
      evidence: browserRow2,
      kind: 'evidence_artifact_content_chunk',
      nextOffset: null,
      offset: 0,
      previewMode: artifact.previewMode,
      readCursor: readCursor(),
      returnedBytes: artifact.sizeBytes,
      state: 'available',
      totalBytes: artifact.sizeBytes,
      truncated: false,
    },
    page: { hasMore: false, nextCursor: null },
  }
}

function artifactClient(artifact, chunkOverrides = {}) {
  return new FakeControlPlaneClient(request => {
    if (request.query === QueryName.EvidenceGet) {
      return evidenceGet(request, evidenceDetailResult({
        artifactAccess: { state: 'available', items: [artifact] },
      }))
    }
    return chunkResponse(request, { artifact, ...chunkOverrides })
  })
}

const imageClient = () => artifactClient(descriptor())

function viewModel(overrides = {}) {
  const model = overrides.model ?? new FakeStrongFlowModel()
  const client = overrides.client ?? new FakeControlPlaneClient()
  const link = overrides.deepLink ?? deepLink()
  const created = createStrongFlowEvidenceViewModel({
    client,
    actor,
    scope,
    nextRequestId,
    model,
    route: link.route,
    onRouteChange: link.onRouteChange,
    ...(overrides.options ?? {}),
  })
  return { created, client, model, link }
}

class FakeElement {
  constructor(ownerDocument, tagName) {
    this.ownerDocument = ownerDocument
    this.tagName = tagName.toUpperCase()
  }

  attributes = new Map()
  children = []
  parentNode = null
  listeners = new Map()
  dataset = {}
  className = ''
  disabled = false
  hidden = false
  type = ''
  value = ''
  id = ''
  tabIndex = 0
  #textContent = ''

  get textContent() {
    return this.#textContent
  }

  set textContent(value) {
    this.#textContent = String(value)
    this.replaceChildren()
  }

  get childNodes() { return this.children }

  append(...children) {
    for (const child of children) this.insertBefore(child, null)
  }

  replaceChildren(...children) {
    for (const child of [...this.children]) child.remove()
    for (const child of children) this.insertBefore(child, null)
  }

  insertBefore(child, reference) {
    child.remove?.()
    const index = reference === null ? this.children.length : this.children.indexOf(reference)
    this.children.splice(index < 0 ? this.children.length : index, 0, child)
    child.parentNode = this
    return child
  }

  remove() {
    if (this.parentNode === null) return
    const index = this.parentNode.children.indexOf(this)
    if (index >= 0) this.parentNode.children.splice(index, 1)
    this.parentNode = null
  }

  setAttribute(name, value) {
    this.attributes.set(name, String(value))
  }

  getAttribute(name) {
    return this.attributes.get(name) ?? null
  }

  removeAttribute(name) {
    this.attributes.delete(name)
  }

  addEventListener(name, listener) {
    const listeners = this.listeners.get(name) ?? []
    listeners.push(listener)
    this.listeners.set(name, listeners)
  }

  removeEventListener(name, listener) {
    const listeners = this.listeners.get(name) ?? []
    this.listeners.set(name, listeners.filter(candidate => candidate !== listener))
  }

  emit(name, values = {}) {
    for (const listener of this.listeners.get(name) ?? []) listener(values)
  }

  focus() {}

  blur() {}
}

class FakeDocument {
  activeElement = null

  createElement(tagName) {
    return new FakeElement(this, tagName)
  }
}

function findByClass(node, className) {
  if (node.className === className) return node
  for (const child of node.children) {
    const match = findByClass(child, className)
    if (match !== null) return match
  }
  return null
}

function findAllByClass(node, className, matches = []) {
  if (node.className === className) matches.push(node)
  for (const child of node.children) findAllByClass(child, className, matches)
  return matches
}

function mountedWorkbench(overrides = {}) {
  const document = new FakeDocument()
  const rootElement = document.createElement('section')
  const model = overrides.model ?? new FakeStrongFlowModel()
  const client = overrides.client ?? new FakeControlPlaneClient(request => evidenceGet(
    request,
    evidenceDetailResult(),
  ))
  const link = overrides.deepLink ?? deepLink()
  const mounted = mountStrongFlowEvidence({
    root: rootElement,
    model,
    client,
    actor,
    scope,
    nextRequestId,
    route: link.route,
    onRouteChange: link.onRouteChange,
    ...(overrides.limits === undefined ? {} : { limits: overrides.limits }),
    ...(overrides.objectUrls === undefined ? {} : { objectUrls: overrides.objectUrls }),
  })
  return { document, rootElement, model, client, link, mounted }
}

function openFirstPreviewRow(rootElement) {
  // The open control lives inside one clickable Preview row.
  findAllByClass(rootElement, 'wwc-strongflow-preview-row')[0].emit('click')
}

function previewTabOf(rootElement) {
  const tabs = findByClass(rootElement, 'wwc-strongflow-evidence-tabs').children
  assert.deepEqual(tabs.map(tab => tab.textContent), ['Evidence', 'Preview', 'Tests', 'Logs'])
  tabs[1].emit('click')
  assert.equal(findAllByClass(rootElement, 'wwc-strongflow-evidence-panel')[1].hidden, false)
  return tabs
}

test('preview validity and image bounds are explicit bounded constants', () => {
  assert.equal(STRONGFLOW_PREVIEW_VALIDITY_MILLIS, 24 * 60 * 60 * 1000)
  assert.equal(MAX_PREVIEW_IMAGE_BYTES, 8 * 1024 * 1024)
  assert.equal(DEFAULT_PREVIEW_ROW_LIMIT > 0 && DEFAULT_PREVIEW_ROW_LIMIT <= 500, true)
})

test('the Preview tab keeps the runtime and test Evidence that can carry a Preview', () => {
  const rows = browserProjection().evidence
  assert.deepEqual(strongFlowPreviewRowsForTab(rows).map(row => row.id), [
    evidenceId(1),
    evidenceId(2),
    evidenceId(4),
  ])
  assert.deepEqual(strongFlowPreviewRowsForTab([]), [])
})

test('only raster image media types may be opened as a Preview screenshot', () => {
  assert.equal(strongFlowPreviewImageSupport('image/png'), 'renderable')
  assert.equal(strongFlowPreviewImageSupport('IMAGE/JPEG'), 'renderable')
  assert.equal(strongFlowPreviewImageSupport('image/webp'), 'renderable')
  assert.equal(strongFlowPreviewImageSupport('image/gif'), 'renderable')
  assert.equal(strongFlowPreviewImageSupport('image/png; charset=binary'), 'renderable')
  // SVG is an image media type that executes markup, so it can never be opened.
  assert.equal(strongFlowPreviewImageSupport('image/svg+xml'), 'scriptable')
  assert.equal(strongFlowPreviewImageSupport('text/html'), 'unsupported')
  assert.equal(strongFlowPreviewImageSupport('image/tiff'), 'unsupported')
  assert.equal(strongFlowPreviewImageSupport(''), 'unsupported')
})

test('the open channel follows the authority preview mode and safe content rules', () => {
  assert.deepEqual(
    strongFlowPreviewChannel(descriptor({ mediaType: 'text/plain' }), 'utf-8', 32),
    { channel: 'text', reason: 'inline-text' },
  )
  assert.deepEqual(
    strongFlowPreviewChannel(descriptor(), 'binary', PNG_BYTES.length),
    { channel: 'image', reason: 'raster-image' },
  )
  assert.deepEqual(
    strongFlowPreviewChannel(descriptor(), 'utf-8', PNG_BYTES.length),
    { channel: 'download-only', reason: 'encoding' },
  )
  assert.deepEqual(
    strongFlowPreviewChannel(
      descriptor({ mediaType: 'image/svg+xml' }),
      'binary',
      PNG_BYTES.length,
    ),
    { channel: 'download-only', reason: 'scriptable-image' },
  )
  assert.deepEqual(
    strongFlowPreviewChannel(descriptor({ mediaType: 'image/tiff' }), 'binary', 4),
    { channel: 'download-only', reason: 'unsupported-media-type' },
  )
  assert.deepEqual(
    strongFlowPreviewChannel(descriptor(), 'binary', MAX_PREVIEW_IMAGE_BYTES + 1),
    { channel: 'download-only', reason: 'oversized' },
  )
  assert.deepEqual(
    strongFlowPreviewChannel(descriptor({ previewMode: 'download_only' }), 'binary', 4),
    { channel: 'download-only', reason: 'download-only-preview' },
  )
})

test('Preview health reports every closed state and never invents a pass', () => {
  const cases = [
    ['unreachable', strongFlowPreviewSnapshot(null), null],
    ['no-candidate', strongFlowPreviewSnapshot(browserProjection({ currentCandidate: null })), null],
    [
      'not-generated',
      strongFlowPreviewSnapshot(browserProjection({
        evidence: [evidenceRow(3, { type: 'command' })],
      })),
      null,
    ],
    [
      'expired',
      strongFlowPreviewSnapshot(browserProjection({
        evidence: [evidenceRow(1, { type: 'test', createdAt: '2026-09-01T00:00:00.000Z' })],
      })),
      null,
    ],
    ['unverified', strongFlowPreviewSnapshot(browserProjection()), null],
    [
      'degraded',
      strongFlowPreviewSnapshot(browserProjection({
        verdict: {
          status: 'fail',
          producedAt: snapshotUpdatedAt,
          criteria: [
            criterion('criterion:console', 'fail', [evidenceId(2)]),
            criterion('criterion:browser', 'pass', [evidenceId(1)]),
          ],
        },
      })),
      null,
    ],
    ['healthy', strongFlowPreviewSnapshot(browserProjection({ verdict: passingVerdict() })), null],
    [
      'unreachable',
      strongFlowPreviewSnapshot(
        browserProjection({ verdict: passingVerdict() }),
        { connection: { viewStatus: 'ready', realtime: 'reconnecting' } },
      ),
      'a reconnecting realtime stream is not a Preview pass',
    ],
    [
      'unreachable',
      strongFlowPreviewSnapshot(
        browserProjection({ verdict: passingVerdict() }),
        { connection: { viewStatus: 'error', realtime: 'subscribed' } },
      ),
      'a failed StrongFlow read is not a Preview pass',
    ],
    [
      'unreachable',
      strongFlowPreviewSnapshot(
        browserProjection({ verdict: passingVerdict() }),
        { connection: { viewStatus: 'refreshing', realtime: 'reloading' } },
      ),
      'a snapshot reload is not a Preview pass',
    ],
  ]
  const seen = new Set()
  for (const [expected, snapshot, message] of cases) {
    assert.equal(snapshot.health.id, expected, message ?? expected)
    assert.equal(snapshot.health.pass, expected === 'healthy', `${expected} must not report a pass`)
    assert.notEqual(snapshot.health.label, '')
    assert.notEqual(snapshot.health.detail, '')
    seen.add(expected)
  }
  assert.deepEqual([...seen].sort(), [
    'degraded',
    'expired',
    'healthy',
    'no-candidate',
    'not-generated',
    'unreachable',
    'unverified',
  ].sort())
})

test('the Preview screenshot note names what the opened Evidence actually holds', () => {
  assert.match(
    strongFlowPreviewScreenshotNote(null),
    /Open a Preview record/u,
  )
  assert.match(
    strongFlowPreviewScreenshotNote({ evidenceId: evidenceId(5), status: null }),
    /No screenshot Artifact is linked/u,
  )
  assert.match(
    strongFlowPreviewScreenshotNote({ evidenceId: evidenceId(5), status: 'unavailable' }),
    /no authoritative Artifact link/u,
  )
  assert.equal(
    strongFlowPreviewScreenshotNote({ evidenceId: evidenceId(5), status: 'image' }),
    `The screenshot for Evidence ${evidenceId(5)} is open in the Evidence detail viewer.`,
  )
  assert.match(
    strongFlowPreviewScreenshotNote({ evidenceId: evidenceId(5), status: 'download-only' }),
    /download control/u,
  )
  assert.match(
    strongFlowPreviewScreenshotNote({ evidenceId: evidenceId(5), status: 'error' }),
    /could not be read/u,
  )
  assert.match(
    strongFlowPreviewScreenshotNote({ evidenceId: evidenceId(5), status: 'loading' }),
    /Loading the screenshot/u,
  )
})

test('Preview health measures validity against the snapshot, not the wall clock', () => {
  const snapshot = strongFlowPreviewSnapshot(browserProjection(), {
    validityMillis: 60 * 60 * 1000 - 1,
  })
  // The newest browser Evidence is 2026-09-02T11:00:00Z and the snapshot was
  // written at 12:00Z, so one hour of validity is exhausted to the millisecond.
  assert.equal(snapshot.health.id, 'expired')
  assert.equal(snapshot.newestEvidenceAt, '2026-09-02T11:00:00.000Z')
  assert.equal(snapshot.validityMillis, 60 * 60 * 1000 - 1)

  const unparseable = strongFlowPreviewSnapshot(browserProjection({
    evidence: [evidenceRow(1, { type: 'test', createdAt: 'not-a-timestamp' })],
  }))
  assert.equal(unparseable.health.id, 'expired')

  const justInside = strongFlowPreviewSnapshot(browserProjection({
    evidence: [evidenceRow(1, { type: 'test', createdAt: snapshotUpdatedAt })],
  }), { validityMillis: 1 })
  assert.equal(justInside.health.id, 'unverified')
  assert.equal(justInside.health.pass, false)
})

test('Preview rows stay bounded, newest first, and exclude superseded Candidate facts', () => {
  const projection = browserProjection({
    evidence: [
      evidenceRow(1, { type: 'test', createdAt: '2026-09-02T09:00:00.000Z' }),
      evidenceRow(2, { createdAt: '2026-09-02T11:00:00.000Z' }),
      evidenceRow(3, {
        type: 'runtime_event',
        createdAt: '2026-09-02T10:00:00.000Z',
        candidateRef: supersededCandidateRef,
      }),
      evidenceRow(4, { type: 'test', createdAt: '2026-09-02T11:30:00.000Z' }),
    ],
  })
  const snapshot = strongFlowPreviewSnapshot(projection, { limit: 2 })
  assert.deepEqual(snapshot.items.map(item => item.row.id), [evidenceId(4), evidenceId(2)])
  assert.deepEqual(snapshot.items.map(item => item.kind), ['test-run', 'runtime-log'])
  assert.equal(snapshot.omitted, 1)
  assert.equal(snapshot.supersededCount, 1)
  assert.equal(snapshot.candidateState, 'current')

  const supersededOnly = strongFlowPreviewSnapshot(browserProjection({
    evidence: [evidenceRow(1, { type: 'test', candidateRef: supersededCandidateRef })],
  }))
  assert.equal(supersededOnly.supersededCount, 1)
  assert.equal(supersededOnly.candidateState, 'current')
  assert.equal(supersededOnly.health.id, 'not-generated')
})

test('Preview joins Criterion results to each browser Evidence row', () => {
  const projection = browserProjection({
    verdict: {
      status: 'fail',
      producedAt: snapshotUpdatedAt,
      criteria: [
        criterion('criterion:console', 'fail', [evidenceId(2)]),
        criterion('criterion:browser', 'pass', [evidenceId(1)]),
        criterion('criterion:foreign', 'fail', ['evd_0000000000000000000000000Z']),
      ],
    },
  })
  const snapshot = strongFlowPreviewSnapshot(projection)
  const byId = new Map(snapshot.items.map(item => [item.row.id, item]))
  assert.deepEqual(byId.get(evidenceId(1)).criterionIds, ['criterion:browser'])
  assert.deepEqual(byId.get(evidenceId(1)).failingCriterionIds, [])
  assert.deepEqual(byId.get(evidenceId(2)).criterionIds, ['criterion:console'])
  assert.deepEqual(byId.get(evidenceId(2)).failingCriterionIds, ['criterion:console'])
  assert.equal(snapshot.health.id, 'degraded')
})

test('a raster screenshot artifact is retained as bounded bytes for the sandboxed viewer', async () => {
  const { created } = viewModel({ client: imageClient() })
  await created.openEvidence(evidenceId(2))
  assert.equal(created.state.content.status, 'image')
  assert.equal(created.state.content.channelReason, 'raster-image')
  assert.equal(created.state.content.image.mediaType, 'image/png')
  assert.deepEqual([...created.state.content.image.bytes], PNG_BYTES)
  assert.equal(created.state.content.complete, true)
  assert.equal(created.state.content.text, null)
})

test('unsafe or oversized Artifact content stays download-only with the exact reason', async () => {
  const scriptable = viewModel({
    client: artifactClient(descriptor({ mediaType: 'image/svg+xml' })),
  })
  await scriptable.created.openEvidence(evidenceId(2))
  assert.equal(scriptable.created.state.content.status, 'download-only')
  assert.equal(scriptable.created.state.content.channelReason, 'scriptable-image')
  assert.equal(scriptable.created.state.content.image, null)
  assert.equal(scriptable.client.queries.filter(query => (
    query.query === QueryName.EvidenceArtifactContentGet
  )).length, 0)

  const oversizedDescriptor = descriptor({ sizeBytes: MAX_PREVIEW_IMAGE_BYTES + 1 })
  const oversized = new FakeControlPlaneClient(request => {
    if (request.query === QueryName.EvidenceGet) {
      return evidenceGet(request, evidenceDetailResult({
        artifactAccess: { state: 'available', items: [oversizedDescriptor] },
      }))
    }
    return {
      ...chunkResponse(request, { artifact: oversizedDescriptor }),
      result: {
        ...chunkResponse(request, { artifact: oversizedDescriptor }).result,
        dataBase64: Buffer.from(new Uint8Array(MAX_PREVIEW_IMAGE_BYTES + 1)).toString('base64'),
        returnedBytes: MAX_PREVIEW_IMAGE_BYTES + 1,
      },
    }
  })
  const bounded = viewModel({ client: oversized })
  await bounded.created.openEvidence(evidenceId(2))
  assert.equal(bounded.created.state.content.status, 'download-only')
  assert.equal(bounded.created.state.content.channelReason, 'oversized')
  // An Artifact the channel rule rejects is never read at all.
  assert.equal(oversized.queries.filter(query => (
    query.query === QueryName.EvidenceArtifactContentGet
  )).length, 0)

  const gated = viewModel({
    client: artifactClient(descriptor({ previewMode: 'download_only' })),
  })
  await gated.created.openEvidence(evidenceId(2))
  assert.equal(gated.created.state.content.status, 'download-only')
  assert.equal(gated.created.state.content.channelReason, 'download-only-preview')
  assert.equal(gated.created.state.content.image, null)
})

test('the workbench renders one Preview tab with health, reasons, and criterion joins', () => {
  const { rootElement, link } = mountedWorkbench()
  previewTabOf(rootElement)
  const health = findByClass(rootElement, 'wwc-strongflow-preview-health')
  assert.equal(health.dataset.previewHealth, 'unverified')
  assert.equal(health.dataset.wwcComponent, 'status-badge')
  assert.match(findByClass(rootElement, 'wwc-strongflow-preview-reason').textContent, /no Criterion/u)
  assert.match(
    findByClass(rootElement, 'wwc-strongflow-preview-screenshots').textContent,
    /Open a Preview record/u,
  )
  const rows = findAllByClass(rootElement, 'wwc-strongflow-preview-row')
  assert.equal(rows.length, 2)
  // Rows render newest Evidence first.
  assert.deepEqual(rows.map(row => row.dataset.previewKind), ['runtime-log', 'test-run'])
  assert.equal(rows.every(row => row.dataset.candidateState === 'current'), true)
  assert.match(
    findByClass(rows[0], 'wwc-strongflow-preview-criteria').textContent,
    /No current criterion join/u,
  )
  assert.match(link.state.hash, /tab=preview/u)
})

test('Preview rows expose their Criterion verdicts and stay inside their own render limit', () => {
  const projection = browserProjection({
    verdict: {
      status: 'fail',
      producedAt: snapshotUpdatedAt,
      criteria: [
        criterion('criterion:console', 'fail', [evidenceId(2)]),
        criterion('criterion:browser', 'pass', [evidenceId(1)]),
      ],
    },
  })
  const bounded = browserProjection({
    evidence: [
      evidenceRow(1, {
        type: 'test',
        sourceRef: 'test:browser:1',
        createdAt: '2026-09-02T11:30:00.000Z',
      }),
      evidenceRow(2, { createdAt: '2026-09-02T11:00:00.000Z' }),
    ],
    verdict: projection.verdict,
  })
  const { rootElement } = mountedWorkbench({
    model: new FakeStrongFlowModel(modelState({ projection: bounded })),
    limits: { evidence: 100, preview: 1 },
  })
  previewTabOf(rootElement)
  const rows = findAllByClass(rootElement, 'wwc-strongflow-preview-row')
  assert.deepEqual(rows.map(row => row.dataset.previewKind), ['test-run'])
  assert.match(
    findByClass(rows[0], 'wwc-strongflow-preview-criteria').textContent,
    /criterion:browser/u,
  )
  const omitted = findByClass(rootElement, 'wwc-strongflow-preview-omitted')
  assert.match(omitted.textContent, /1 more Preview record/u)
  assert.equal(omitted.hidden, false)
})

test('a disconnected snapshot renders an explicit non-pass Preview health state', () => {
  const { rootElement } = mountedWorkbench({
    model: new FakeStrongFlowModel(modelState({
      status: 'error',
      realtime: 'reconnecting',
      projection: null,
    })),
  })
  previewTabOf(rootElement)
  const health = findByClass(rootElement, 'wwc-strongflow-preview-health')
  assert.equal(health.dataset.previewHealth, 'unreachable')
  assert.equal(health.dataset.tone, 'infra')
  assert.match(
    findByClass(rootElement, 'wwc-strongflow-preview-reason').textContent,
    /not connected/u,
  )
  assert.equal(findAllByClass(rootElement, 'wwc-strongflow-preview-row').length, 0)
})

test('a Preview screenshot opens in one bounded sandboxed image node and is revoked', async () => {
  const created = []
  const revoked = []
  let counter = 0
  const objectUrls = {
    create() {
      counter += 1
      const url = `blob:fixture:${String(counter)}`
      created.push(url)
      return url
    },
    revoke(url) {
      revoked.push(url)
    },
  }
  const { rootElement, mounted } = mountedWorkbench({ client: imageClient(), objectUrls })
  assert.deepEqual(created, [])
  previewTabOf(rootElement)
  openFirstPreviewRow(rootElement)
  await new Promise(resolve => { setImmediate(resolve) })
  await new Promise(resolve => { setImmediate(resolve) })
  const detail = findByClass(rootElement, 'wwc-strongflow-evidence-detail')
  assert.equal(detail.dataset.status, 'ready')
  const image = findByClass(rootElement, 'wwc-strongflow-evidence-image-content')
  assert.notEqual(image, null)
  assert.equal(image.src, 'blob:fixture:1')
  assert.equal(image.getAttribute('referrerpolicy'), 'no-referrer')
  assert.equal(image.getAttribute('decoding'), 'async')
  assert.equal(image.getAttribute('data-preview-sandbox'), 'image-only')
  assert.match(
    findByClass(rootElement, 'wwc-strongflow-evidence-image-caption').textContent,
    /image\/png/u,
  )
  assert.equal(findByClass(rootElement, 'wwc-strongflow-evidence-image').hidden, false)
  const text = findByClass(rootElement, 'wwc-strongflow-evidence-content-text')
  assert.equal(text.hidden, true)
  assert.equal(text.textContent, '')
  assert.deepEqual(revoked, [])
  mounted.close()
  assert.deepEqual(revoked, ['blob:fixture:1'])
})

test('a text Artifact never mounts an image node', async () => {
  const textDescriptor = descriptor({ mediaType: 'text/plain', fileName: 'run.log', sizeBytes: 9 })
  const { rootElement } = mountedWorkbench({
    client: new FakeControlPlaneClient(request => {
      if (request.query === QueryName.EvidenceGet) {
        return evidenceGet(request, evidenceDetailResult({
          artifactAccess: { state: 'available', items: [textDescriptor] },
        }))
      }
      void textDescriptor
      return {
        ...chunkResponse(request, { artifact: textDescriptor }),
        result: {
          ...chunkResponse(request, { artifact: textDescriptor }).result,
          contentEncoding: 'utf-8',
          dataBase64: Buffer.from('log line\n', 'utf8').toString('base64'),
          returnedBytes: 9,
          totalBytes: 9,
        },
      }
    }),
  })
  previewTabOf(rootElement)
  openFirstPreviewRow(rootElement)
  await new Promise(resolve => { setImmediate(resolve) })
  await new Promise(resolve => { setImmediate(resolve) })
  const frame = findByClass(rootElement, 'wwc-strongflow-evidence-image')
  assert.equal(frame.hidden, true)
  assert.equal(frame.getAttribute('src'), null)
  assert.match(findByClass(rootElement, 'wwc-strongflow-evidence-content-text').textContent, /log line/u)
})

test('unavailable Artifact authority names the exact Preview reason', async () => {
  // The default fixture answers with the producer's `no_authoritative_link`.
  const { rootElement } = mountedWorkbench()
  previewTabOf(rootElement)
  openFirstPreviewRow(rootElement)
  await new Promise(resolve => { setImmediate(resolve) })
  await new Promise(resolve => { setImmediate(resolve) })
  const detail = findByClass(rootElement, 'wwc-strongflow-evidence-detail')
  assert.equal(detail.dataset.status, 'ready')
  assert.match(
    findByClass(rootElement, 'wwc-strongflow-evidence-artifact').textContent,
    /No authoritative Artifact link/u,
  )
  assert.match(
    findByClass(rootElement, 'wwc-strongflow-preview-screenshots').textContent,
    /No screenshot Artifact is linked/u,
  )
  assert.equal(findByClass(rootElement, 'wwc-strongflow-evidence-image').hidden, true)
  assert.equal(findByClass(rootElement, 'wwc-strongflow-evidence-image-content').src, undefined)
})

test('a deep link restores the Preview tab and its selected Evidence', async () => {
  const link = deepLink()
  link.state.hash = `#/strongflow?delivery=${deliveryId}&tab=preview&evidence=${evidenceId(2)}`
  const { created } = viewModel({ client: imageClient(), deepLink: link })
  assert.equal(created.state.tab, 'preview')
  assert.equal(created.state.connection.viewStatus, 'ready')
  assert.equal(created.state.connection.realtime, 'subscribed')
  await new Promise(resolve => { setImmediate(resolve) })
  await new Promise(resolve => { setImmediate(resolve) })
  assert.equal(created.state.selected.row.id, evidenceId(2))
  assert.equal(created.state.content.status, 'image')
  assert.match(link.state.hash, /tab=preview/u)
})

test('the Preview route stays canonical and fails closed on an unknown tab', () => {
  const link = deepLink()
  link.state.hash = `#/strongflow?delivery=${deliveryId}&tab=preview`
  const created = viewModel({ deepLink: link }).created
  created.selectTab('preview')
  assert.equal(created.state.tab, 'preview')
  created.selectTab('evidence')
  assert.equal(created.state.tab, 'evidence')
  assert.equal(created.state.connection.realtime, 'subscribed')
})
