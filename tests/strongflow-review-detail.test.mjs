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
    'apps/client/tsconfig.strongflow-review-detail-tests.json',
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
  `StrongFlow review detail did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const detailModule = await import(`${pathToFileURL(resolve(
  root,
  '.cache/strongflow-review-detail-tests/strongflow-review-detail.js',
)).href}`)
const pageModule = await import(`${pathToFileURL(resolve(
  root,
  '.cache/strongflow-review-detail-tests/strongflow-page.js',
)).href}`)

const {
  createStrongFlowReceiptLoader,
  mountStrongFlowReviewDetailPanel,
  strongFlowReviewDetail,
  strongFlowSecretSafeText,
  strongFlowTechnicalSummary,
  strongFlowTechnicalSummaryLines,
} = detailModule
const { mountStrongFlowPage } = pageModule

const deliveryId = 'dlv_00000000000000000000000001'
const stageRunId = 'run_00000000000000000000000001'
const candidateDigest = `sha256:${'3'.repeat(64)}`
const candidateRef = 'git-candidate:sha256:3333333333333333333333333333333333333333333333333333333333333333'
const commitId = '1'.repeat(40)
const treeId = '2'.repeat(40)
const reviewSetDigest = `sha256:${'7'.repeat(64)}`
const publicationSetDigest = `sha256:${'9'.repeat(64)}`
const cursorToken = 'cursor-token-sealed-do-not-copy'
const actor = { kind: 'user', id: 'usr_00000000000000000000000001' }
const scope = {
  kind: 'repository',
  organizationId: 'org_00000000000000000000000001',
  workspaceId: 'wsp_00000000000000000000000001',
  projectId: 'prj_00000000000000000000000001',
  repositoryId: 'rep_00000000000000000000000001',
}

function evidenceRecord(id, type, sourceRef) {
  return {
    candidateRef,
    createdAt: '2026-09-02T01:00:04.000Z',
    deliverySpecId: 'spec:1',
    deliverySpecRevision: 3,
    id,
    sessionBindingId: 'sess:1',
    sourceRef,
    stageRunId,
    type,
  }
}

/** A verified StrongFlow projection with criteria, verdict, evidence and a Publication. */
function projection(overrides = {}) {
  const delivery = {
      schemaVersion: 'winwincode/v1',
      deliveryId,
      deliveryRevision: 4,
      status: 'verifying',
      ownership: scope,
      requirements: {
        title: 'Bounded StrongFlow delivery',
        goal: 'Deliver one reviewed candidate.',
        deliverySpecId: 'spec:1',
        deliverySpecRevision: 3,
        acceptanceCriteria: [
          {
            id: 'criterion:1',
            description: 'The loop is bounded.',
            required: true,
            verificationMethod: 'unit test',
          },
          {
            id: 'criterion:2',
            description: 'The export stays secret-safe.',
            required: true,
            verificationMethod: null,
          },
          {
            id: 'criterion:3',
            description: 'The changelog is updated.',
            required: false,
            verificationMethod: 'review',
          },
        ],
      },
      tasks: [],
      stages: [{ id: stageRunId, stage: 'verifying', role: 'verifier', status: 'running' }],
      attention: [],
      evidence: [
        evidenceRecord('ev:1', 'test', 'artifact:sha256:aaaa'),
        evidenceRecord('ev:2', 'command', 'artifact:sha256:bbbb'),
      ],
      verdict: {
        id: 'verdict:1',
        candidateRef,
        deliverySpecId: 'spec:1',
        deliverySpecRevision: 3,
        status: 'fail',
        producedAt: '2026-09-02T01:00:05.000Z',
        unresolvedFindings: ['Finding A'],
        criteria: [
          {
            criterionId: 'criterion:1',
            evaluatedAt: '2026-09-02T01:00:05.000Z',
            evidenceRefs: ['ev:1', 'ev:missing'],
            explanation: 'The exact check passed.',
            resultId: 'result:1',
            verdict: 'pass',
          },
          {
            criterionId: 'criterion:2',
            evaluatedAt: '2026-09-02T01:00:05.000Z',
            evidenceRefs: ['ev:2'],
            explanation: 'Expected a redacted report and saw one leaked value.',
            resultId: 'result:2',
            verdict: 'infra_error',
          },
          {
            criterionId: 'criterion:ghost',
            evaluatedAt: '2026-09-02T01:00:05.000Z',
            evidenceRefs: [],
            explanation: 'Reported without a spec criterion.',
            resultId: 'result:3',
            verdict: 'inconclusive',
          },
        ],
      },
      currentCandidate: {
        candidateRef,
        candidateCommitId: commitId,
        candidateTreeId: treeId,
        diffSha256: candidateDigest,
        frozenAt: '2026-09-02T01:00:04.000Z',
      },
      publication: {
        approvalAttentionItemId: 'attention:1',
        approvedAt: '2026-09-02T01:00:06.000Z',
        approvedBy: 'usr_00000000000000000000000002',
        candidateRef,
        deliveryId,
        deliverySpecId: 'spec:1',
        deliverySpecRevision: 3,
        deliveryVerdictId: 'verdict:1',
        id: 'pub_00000000000000000000000001',
        publicationSetSha256: publicationSetDigest,
        resourceRef: { kind: 'github_pull_request', number: 12, repository: 'owner/repo' },
        revision: 1,
        state: 'pending',
        target: {
          baseBranch: 'main',
          headBranch: 'winwincode/candidate-1',
          headRepository: 'owner/repo',
          provider: 'github',
          repository: 'owner/repo',
        },
        updatedAt: '2026-09-02T01:00:06.000Z',
        verdictStatus: 'pass',
      },
      readCursor: { publicationRevision: 1 },
  }
  return {
    delivery,
    solutionReview: {
      reviewStatus: 'approved',
      attentionItemId: 'attention:1',
      deliverySpecId: 'spec:1',
      deliverySpecRevision: 3,
      reviewStageRunId: stageRunId,
      reviewSetSha256: reviewSetDigest,
      architectureDiagram: { nodes: [], edges: [] },
      processDiagram: { nodes: [], edges: [] },
    },
    diagramExecution: null,
    stage: { id: stageRunId },
    runtime: { stageRunId, sessions: [] },
    evidence: delivery.evidence,
    verdict: delivery.verdict,
    attention: delivery.attention,
    currentCandidate: delivery.currentCandidate,
    publication: delivery.publication,
    metadata: {
      source: 'control-plane-snapshot',
      updatedAt: '2026-09-02T01:00:06.000Z',
      revisions: { delivery: 4, deliverySpec: 3, runtime: 8, publication: 1 },
      readCursor: { token: cursorToken },
    },
    ...overrides,
  }
}

function detailOf(overrides = {}, options = {}) {
  return strongFlowReviewDetail(projection(overrides), options)
}

test('every acceptance criterion exposes its outcome and its direct Evidence', () => {
  const detail = detailOf()

  assert.equal(detail.criteria.length, 3)
  assert.deepEqual(detail.criteria.map(row => row.criterionId), [
    'criterion:1',
    'criterion:2',
    'criterion:3',
  ])

  const [first, second, third] = detail.criteria
  assert.equal(first.outcome, 'pass')
  assert.equal(first.required, true)
  assert.equal(first.verificationMethod, 'unit test')
  assert.equal(first.explanation, 'The exact check passed.')
  assert.equal(first.evaluatedAt, '2026-09-02T01:00:05.000Z')
  // Direct Evidence stays limited to records present at this read cursor, so
  // every offered entry point opens a real record.
  assert.deepEqual(first.evidence.map(entry => entry.evidenceId), ['ev:1'])
  assert.equal(first.evidence[0].openable, true)
  assert.equal(first.evidence[0].sourceRef, 'artifact:sha256:aaaa')

  assert.equal(second.outcome, 'infra_error')
  assert.equal(second.required, true)
  assert.deepEqual(second.evidence.map(entry => entry.evidenceId), ['ev:2'])

  // A criterion without a result is reported, never dropped or invented.
  assert.equal(third.outcome, 'not_evaluated')
  assert.equal(third.explanation, null)
  assert.deepEqual(third.evidence, [])

  // A Verdict result without an acceptance criterion is surfaced, not hidden.
  assert.deepEqual(detail.unmatchedResults, [{
    criterionId: 'criterion:ghost',
    outcome: 'inconclusive',
    explanation: 'Reported without a spec criterion.',
  }])
})

test('an acceptance criterion without a verdict result is reported as not evaluated', () => {
  const detail = detailOf({
    verdict: {
      id: 'verdict:1',
      candidateRef,
      deliverySpecId: 'spec:1',
      deliverySpecRevision: 3,
      status: 'inconclusive',
      producedAt: '2026-09-02T01:00:05.000Z',
      unresolvedFindings: [],
      criteria: [],
    },
  })

  assert.deepEqual(detail.criteria.map(row => row.outcome), [
    'not_evaluated',
    'not_evaluated',
    'not_evaluated',
  ])
  assert.equal(detail.verdict.status, 'inconclusive')
})

test('verdict reasons name every non-pass criterion and keep unresolved findings visible', () => {
  const detail = detailOf()

  assert.equal(detail.verdict.verdictId, 'verdict:1')
  assert.equal(detail.verdict.status, 'fail')
  assert.equal(detail.verdict.producedAt, '2026-09-02T01:00:05.000Z')
  assert.equal(detail.verdict.candidateRef, candidateRef)
  assert.deepEqual(detail.verdict.unresolvedFindings, ['Finding A'])
  assert.deepEqual(detail.verdict.reasons, [
    {
      criterionId: 'criterion:2',
      outcome: 'infra_error',
      explanation: 'Expected a redacted report and saw one leaked value.',
    },
    {
      criterionId: 'criterion:ghost',
      outcome: 'inconclusive',
      explanation: 'Reported without a spec criterion.',
    },
  ])
})

test('a passing verdict reports no reasons and keeps its criteria reviewable', () => {
  const detail = detailOf({
    verdict: {
      id: 'verdict:2',
      candidateRef,
      deliverySpecId: 'spec:1',
      deliverySpecRevision: 3,
      status: 'pass',
      producedAt: '2026-09-02T01:00:07.000Z',
      unresolvedFindings: [],
      criteria: [{
        criterionId: 'criterion:1',
        evaluatedAt: '2026-09-02T01:00:07.000Z',
        evidenceRefs: ['ev:1'],
        explanation: 'The exact check passed.',
        resultId: 'result:1',
        verdict: 'pass',
      }],
    },
  })

  assert.equal(detail.verdict.status, 'pass')
  assert.deepEqual(detail.verdict.reasons, [])
  assert.deepEqual(detail.verdict.unresolvedFindings, [])
  assert.deepEqual(detail.criteria.map(row => row.outcome), [
    'pass',
    'not_evaluated',
    'not_evaluated',
  ])
})

test('the delivery conclusion is one line that names status, verdict and publication', () => {
  assert.equal(
    detailOf().conclusion,
    'Delivery verifying · Verdict failed · Publication pending',
  )
  assert.equal(
    detailOf({ publication: null }).conclusion,
    'Delivery verifying · Verdict failed · Publication not created',
  )
  assert.equal(
    detailOf({ verdict: null }).conclusion,
    'Delivery verifying · Verdict not computed · Publication pending',
  )
  assert.equal(
    detailOf({
      verdict: {
        id: 'verdict:3',
        candidateRef,
        deliverySpecId: 'spec:1',
        deliverySpecRevision: 3,
        status: 'pass',
        producedAt: '2026-09-02T01:00:07.000Z',
        unresolvedFindings: [],
        criteria: [],
      },
      publication: {
        approvalAttentionItemId: 'attention:1',
        approvedAt: '2026-09-02T01:00:08.000Z',
        approvedBy: 'usr_00000000000000000000000002',
        candidateRef,
        deliveryId,
        deliverySpecId: 'spec:1',
        deliverySpecRevision: 3,
        deliveryVerdictId: 'verdict:3',
        id: 'pub_00000000000000000000000001',
        publicationSetSha256: publicationSetDigest,
        resourceRef: null,
        revision: 2,
        state: 'published',
        target: {
          baseBranch: 'main',
          headBranch: 'winwincode/candidate-1',
          headRepository: 'owner/repo',
          provider: 'github',
          repository: 'owner/repo',
        },
        updatedAt: '2026-09-02T01:00:08.000Z',
        verdictStatus: 'pass',
      },
    }).conclusion,
    'Delivery verifying · Verdict passed · Publication published',
  )
})

test('technical identity carries the candidate ref, commit, tree, digests and Evidence references', () => {
  const detail = detailOf()

  assert.deepEqual(detail.technical.entries, [
    { label: 'Delivery', value: `${deliveryId} r4` },
    { label: 'Delivery spec', value: 'spec:1 r3' },
    { label: 'Candidate reference', value: candidateRef },
    { label: 'Candidate commit', value: commitId },
    { label: 'Candidate tree', value: treeId },
    { label: 'Candidate Diff digest', value: candidateDigest },
    { label: 'Approved review set digest', value: reviewSetDigest },
    { label: 'Publication set digest', value: publicationSetDigest },
  ])
  assert.deepEqual(detail.technical.evidenceReferences, [
    { evidenceId: 'ev:1', type: 'test', sourceRef: 'artifact:sha256:aaaa', openable: true },
    { evidenceId: 'ev:2', type: 'command', sourceRef: 'artifact:sha256:bbbb', openable: true },
  ])
  assert.equal(detail.technical.omittedEvidenceReferences, 0)
})

test('technical identity stays bounded and drops the candidate-less rows', () => {
  const detail = strongFlowReviewDetail(
    projection({
      currentCandidate: null,
      solutionReview: null,
      publication: null,
      evidence: Array.from({ length: 5 }, (_, index) => evidenceRecord(
        `ev:${String(index + 1)}`,
        'test',
        `artifact:sha256:${String(index).repeat(4)}`,
      )),
    }),
    { limits: { evidence: 3 } },
  )

  assert.deepEqual(detail.technical.entries.map(entry => entry.label), [
    'Delivery',
    'Delivery spec',
  ])
  assert.equal(detail.technical.evidenceReferences.length, 3)
  assert.equal(detail.technical.omittedEvidenceReferences, 2)
})

test('the publication receipt exposes identity, external reference and traceability', () => {
  const detail = detailOf()
  assert.equal(detail.publication.present, true)
  assert.equal(detail.publication.publicationId, 'pub_00000000000000000000000001')
  assert.equal(detail.publication.state, 'pending')
  assert.equal(detail.publication.revision, 1)
  assert.equal(detail.publication.deliveryVerdictId, 'verdict:1')
  assert.equal(detail.publication.approvedBy, 'usr_00000000000000000000000002')
  assert.equal(detail.publication.publicationSetSha256, publicationSetDigest)
  assert.deepEqual(detail.publication.externalReferences, [{
    kind: 'github_pull_request',
    repository: 'owner/repo',
    number: 12,
  }])
  // Traceability flags come only from the verified Publication journal, so
  // they stay unknown until that detail is loaded.
  assert.equal(detail.publication.detailStatus, 'not_loaded')
  assert.equal(detail.publication.retryable, null)
  assert.equal(detail.publication.cancellable, null)
  assert.deepEqual(detail.publication.history, [])
  assert.deepEqual(detail.publication.steps, [])

  const withoutPublication = detailOf({ publication: null })
  assert.equal(withoutPublication.publication.present, false)
  assert.deepEqual(withoutPublication.publication.externalReferences, [])
})

test('a loaded publication detail adds retry, cancel and step traceability', () => {
  const detail = detailOf({}, {
    receiptDetail: {
      cancellable: true,
      cancellation: null,
      history: [
        {
          cancellable: true,
          retryable: false,
          revision: 1,
          state: 'pending',
          stepStates: [{ kind: 'branch', state: 'pending' }],
          updatedAt: '2026-09-02T01:00:06.000Z',
        },
        {
          cancellable: false,
          retryable: true,
          revision: 2,
          state: 'failed',
          stepStates: [{ kind: 'pull_request', state: 'rejected' }],
          updatedAt: '2026-09-02T01:00:09.000Z',
        },
      ],
      historyTruncated: false,
      kind: 'publication_detail',
      retryable: true,
      steps: [{
        kind: 'pull_request',
        outcomeCode: 'RESOURCE_CONFLICT',
        remoteWritePerformed: false,
        resourceRef: { kind: 'github_pull_request', number: 12, repository: 'owner/repo' },
        retryable: true,
        state: 'rejected',
      }],
      summary: {
        approvalAttentionItemId: 'attention:1',
        approvedAt: '2026-09-02T01:00:06.000Z',
        approvedBy: 'usr_00000000000000000000000002',
        candidateRef,
        deliveryId,
        deliverySpecId: 'spec:1',
        deliverySpecRevision: 3,
        deliveryVerdictId: 'verdict:1',
        id: 'pub_00000000000000000000000001',
        publicationSetSha256: publicationSetDigest,
        resourceRef: { kind: 'github_pull_request', number: 12, repository: 'owner/repo' },
        revision: 2,
        state: 'failed',
        target: {
          baseBranch: 'main',
          headBranch: 'winwincode/candidate-1',
          headRepository: 'owner/repo',
          provider: 'github',
          repository: 'owner/repo',
        },
        updatedAt: '2026-09-02T01:00:09.000Z',
        verdictStatus: 'pass',
      },
    },
  })

  assert.equal(detail.publication.detailStatus, 'ready')
  assert.equal(detail.publication.retryable, true)
  assert.equal(detail.publication.cancellable, true)
  assert.equal(detail.publication.state, 'failed')
  assert.equal(detail.publication.revision, 2)
  assert.deepEqual(detail.publication.history.map(entry => entry.revision), [1, 2])
  assert.deepEqual(detail.publication.steps, [{
    kind: 'pull_request',
    state: 'rejected',
    outcomeCode: 'RESOURCE_CONFLICT',
    remoteWritePerformed: false,
    resourceRef: { kind: 'github_pull_request', number: 12, repository: 'owner/repo' },
    retryable: true,
  }])
})

test('a receipt detail for another publication fails closed instead of being shown', () => {
  const detail = detailOf({}, {
    receiptDetail: {
      cancellable: false,
      cancellation: null,
      history: [],
      historyTruncated: false,
      kind: 'publication_detail',
      retryable: true,
      steps: [],
      summary: {
        approvalAttentionItemId: 'attention:1',
        approvedAt: '2026-09-02T01:00:06.000Z',
        approvedBy: 'usr_00000000000000000000000002',
        candidateRef,
        deliveryId: 'dlv_00000000000000000000000002',
        deliverySpecId: 'spec:1',
        deliverySpecRevision: 3,
        deliveryVerdictId: 'verdict:1',
        id: 'pub_00000000000000000000000009',
        publicationSetSha256: publicationSetDigest,
        resourceRef: null,
        revision: 7,
        state: 'published',
        target: {
          baseBranch: 'main',
          headBranch: 'winwincode/candidate-1',
          headRepository: 'owner/repo',
          provider: 'github',
          repository: 'owner/repo',
        },
        updatedAt: '2026-09-02T01:00:09.000Z',
        verdictStatus: 'pass',
      },
    },
  })

  assert.equal(detail.publication.detailStatus, 'error')
  assert.equal(detail.publication.retryable, null)
  assert.equal(detail.publication.cancellable, null)
  assert.deepEqual(detail.publication.history, [])
  assert.deepEqual(detail.publication.steps, [])
})

test('secret-safe text redacts credential shapes and keeps verification digests', () => {
  const redacted = strongFlowSecretSafeText(
    'provider token sk-ABCDEF1234567890abc inside the log, github ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ123456 too',
  )
  assert.equal(redacted.includes('sk-ABCDEF1234567890abc'), false)
  assert.equal(redacted.includes('ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ123456'), false)
  assert.equal(redacted.includes('[redacted]'), true)

  const digest = strongFlowSecretSafeText(`keep ${candidateDigest} and refs/heads/main`)
  assert.equal(digest, `keep ${candidateDigest} and refs/heads/main`)
})

test('the technical summary is copyable, closed and secret-safe', () => {
  const leakedFinding = 'Finding with ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ123456 inside'
  const detail = detailOf({
    verdict: {
      id: 'verdict:1',
      candidateRef,
      deliverySpecId: 'spec:1',
      deliverySpecRevision: 3,
      status: 'fail',
      producedAt: '2026-09-02T01:00:05.000Z',
      unresolvedFindings: [leakedFinding],
      criteria: [{
        criterionId: 'criterion:2',
        evaluatedAt: '2026-09-02T01:00:05.000Z',
        evidenceRefs: ['ev:2'],
        explanation: 'Expected a redacted report and saw one leaked value.',
        resultId: 'result:2',
        verdict: 'infra_error',
      }],
    },
  })

  const lines = strongFlowTechnicalSummaryLines(detail)
  assert.equal(lines[0], 'StrongFlow technical summary')
  assert.equal(lines.includes(`Candidate reference: ${candidateRef}`), true)
  assert.equal(lines.includes(`Candidate commit: ${commitId}`), true)
  assert.equal(lines.includes(`Candidate tree: ${treeId}`), true)
  assert.equal(lines.includes(`Candidate Diff digest: ${candidateDigest}`), true)
  assert.equal(lines.includes('Evidence (2):'), true)
  assert.equal(lines.includes('  - test ev:1 artifact:sha256:aaaa'), true)
  assert.equal(lines.includes('Verdict: failed'), true)
  assert.equal(lines.includes('  - criterion:2 infra_error: Expected a redacted report and saw one leaked value.'), true)
  assert.equal(lines.includes('Publication: pending'), true)

  const text = strongFlowTechnicalSummary(detail)
  assert.equal(text, lines.join('\n'))
  // The sealed read-cursor token never reaches the copyable text.
  assert.equal(text.includes(cursorToken), false)
  // Credential-shaped values are redacted, not carried out of the browser.
  assert.equal(text.includes('ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ123456'), false)
  assert.equal(text.includes('[redacted]'), true)
})

test('the receipt loader issues exactly one typed publication query', async () => {
  const queries = []
  const loader = createStrongFlowReceiptLoader({
    client: {
      async query(request) {
        queries.push(request)
        return {
          schemaVersion: 'winwincode/v1',
          requestId: request.requestId,
          query: request.query,
          result: receiptDetail({ summary: { revision: 3, state: 'publishing' } }),
          page: { hasMore: false, nextCursor: null },
        }
      },
    },
    actor,
    scope,
    nextRequestId: () => 'req_00000000000000000000000001',
  })

  const detail = await loader.load('pub_00000000000000000000000001')
  assert.equal(queries.length, 1)
  assert.equal(queries[0].query, 'publication.get')
  assert.equal(queries[0].schemaVersion, 'winwincode/v1')
  assert.deepEqual(queries[0].parameters, { publicationId: 'pub_00000000000000000000000001' })
  assert.deepEqual(queries[0].actor, actor)
  assert.deepEqual(queries[0].scope, scope)
  assert.equal(detail.summary.revision, 3)

  loader.close()
  assert.equal(await loader.load('pub_00000000000000000000000001'), null)
  assert.equal(queries.length, 1)
})

test('the review detail panel is part of the workbench main view', async () => {
  const document = new FakeDocument()
  const rootElement = document.createElement('main')
  const model = new FakeStrongFlowViewModel(reviewState())
  const client = receiptClient()
  const mounted = mountStrongFlowPage({
    root: rootElement,
    model,
    deliveryList: fakeDeliveryList([]),
    evidence: pageEvidenceOptions({ client }),
  })

  const panel = findByClass(rootElement, 'wwc-strongflow-review-detail')
  assert.notEqual(panel, null)
  assert.equal(panel.getAttribute('aria-label'), 'Delivery review detail')

  const conclusion = findByClass(rootElement, 'wwc-strongflow-review-detail-conclusion')
  assert.equal(
    conclusion.textContent,
    'Delivery verifying · Verdict failed · Publication pending',
  )

  // Every section starts collapsed so the main view shows the conclusion only.
  for (const toggle of findAllByClass(rootElement, 'wwc-strongflow-review-detail-toggle')) {
    assert.equal(toggle.getAttribute('aria-expanded'), 'false')
  }
  const collapsedBody = findByClass(rootElement, 'wwc-strongflow-review-detail-body')
  assert.equal(collapsedBody.hidden, true)

  // The panel introduces no second live region: the page keeps exactly one
  // polite status element, and every other node stays silent.
  for (const node of walk(panel)) {
    assert.equal(node.getAttribute?.('aria-live'), null, node.className)
  }
  assert.equal(findByClass(rootElement, 'wwc-strongflow-status').getAttribute('aria-live'), 'polite')

  mounted.close()
  assert.deepEqual(rootElement.children, [])
  assert.deepEqual(model.calls.at(-1), ['close'])
})

test('the panel reveals technical identity, criteria and copyable summary on demand', async () => {
  const document = new FakeDocument()
  const rootElement = document.createElement('main')
  const model = new FakeStrongFlowViewModel(reviewState())
  const copied = []
  const mounted = mountStrongFlowPage({
    root: rootElement,
    model,
    deliveryList: fakeDeliveryList([]),
    evidence: pageEvidenceOptions({ client: receiptClient() }),
    copy: async text => {
      copied.push(text)
    },
  })

  const sections = new Map(
    findAllByClass(rootElement, 'wwc-strongflow-review-detail-section')
      .map(section => [section.dataset.section, section]),
  )
  assert.deepEqual([...sections.keys()].sort(), ['criteria', 'publication', 'technical'])

  toggleOf(sections, 'technical').emit('click')
  const technical = findByClass(rootElement, 'wwc-strongflow-review-detail-technical')
  assert.notEqual(technical, null)
  assert.match(technical.textContent, new RegExp(candidateRef.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')))
  assert.match(technical.textContent, /Candidate commit/u)
  assert.match(technical.textContent, new RegExp(candidateDigest.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')))
  const technicalSection = sections.get('technical')
  assert.match(technicalSection.textContent, /artifact:sha256:aaaa/u)
  assert.match(technicalSection.textContent, /test ev:1 artifact:sha256:aaaa/u)

  toggleOf(sections, 'criteria').emit('click')
  const criteria = findByClass(rootElement, 'wwc-strongflow-review-detail-criteria')
  assert.equal(criteria.children.length, 3)
  assert.equal(criteria.children[0].dataset.outcome, 'pass')
  assert.equal(criteria.children[1].dataset.outcome, 'infra_error')
  assert.equal(criteria.children[2].dataset.outcome, 'not_evaluated')
  assert.match(criteria.textContent, /The loop is bounded\./u)
  assert.match(criteria.textContent, /Expected a redacted report and saw one leaked value\./u)
  assert.match(criteria.textContent, /evaluated 2026-09-02T01:00:05\.000Z/u)

  // A Verdict result the spec never asked for is reported, never dropped.
  const unmatched = findByClass(rootElement, 'wwc-strongflow-review-detail-unmatched')
  assert.match(unmatched.textContent, /criterion:ghost/u)

  // Direct Evidence opens through the existing Evidence workbench entry point.
  const evidenceButtons = findAllByClass(
    rootElement,
    'wwc-strongflow-review-detail-criterion-evidence-open',
  )
  assert.equal(evidenceButtons.length, 2)
  evidenceButtons[0].emit('click')
  const evidencePanel = findAllByClass(rootElement, 'wwc-strongflow-artifact-panel')
    .find(panel => panel.dataset.artifactTab === 'evidence')
  assert.equal(evidencePanel?.hidden, false)

  const unresolved = findByClass(rootElement, 'wwc-strongflow-review-detail-findings')
  assert.match(unresolved.textContent, /Finding A/u)
  const reasons = findByClass(rootElement, 'wwc-strongflow-review-detail-reasons')
  assert.match(reasons.textContent, /criterion:2/u)

  findByClass(rootElement, 'wwc-strongflow-review-detail-copy').emit('click')
  await flush()
  const copiedText = copied.at(-1)
  assert.equal(typeof copiedText, 'string')
  assert.match(copiedText, /StrongFlow technical summary/u)
  assert.equal(copiedText.includes(cursorToken), false)
  assert.equal(
    findByClass(rootElement, 'wwc-strongflow-review-detail-summary').value,
    copiedText,
  )

  mounted.close()
  assert.deepEqual(rootElement.children, [])
})

test('the panel loads the publication receipt once and reports retry and cancel', async () => {
  const document = new FakeDocument()
  const rootElement = document.createElement('main')
  const model = new FakeStrongFlowViewModel(reviewState())
  const client = receiptClient()
  const mounted = mountStrongFlowPage({
    root: rootElement,
    model,
    deliveryList: fakeDeliveryList([]),
    evidence: pageEvidenceOptions({ client }),
  })

  const sections = new Map(
    findAllByClass(rootElement, 'wwc-strongflow-review-detail-section')
      .map(section => [section.dataset.section, section]),
  )
  toggleOf(sections, 'publication').emit('click')
  await flush()

  assert.equal(client.queries.length, 1)
  assert.equal(client.queries[0].query, 'publication.get')
  assert.deepEqual(client.queries[0].parameters, {
    publicationId: 'pub_00000000000000000000000001',
  })

  const receipt = findByClass(rootElement, 'wwc-strongflow-review-detail-receipt')
  assert.match(receipt.textContent, /pub_00000000000000000000000001/u)
  assert.match(receipt.textContent, new RegExp(publicationSetDigest.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')))
  assert.match(receipt.textContent, /Retryable/u)
  assert.match(receipt.textContent, /Cancellable/u)
  assert.match(receipt.textContent, /owner\/repo #12/u)
  assert.match(
    findByClass(rootElement, 'wwc-strongflow-review-detail-receipt-history').textContent,
    /r2 publishing/u,
  )
  assert.match(
    findByClass(rootElement, 'wwc-strongflow-review-detail-receipt-steps').textContent,
    /pull_request applying/u,
  )

  // Collapsing and expanding again replays the loaded receipt, not a new query.
  toggleOf(sections, 'publication').emit('click')
  toggleOf(sections, 'publication').emit('click')
  await flush()
  assert.equal(client.queries.length, 1)

  mounted.close()
  assert.deepEqual(rootElement.children, [])
})

test('a failed receipt load stays fail-closed and offers an explicit retry', async () => {
  const document = new FakeDocument()
  const rootElement = document.createElement('main')
  const model = new FakeStrongFlowViewModel(reviewState())
  let shouldFail = true
  const client = {
    queries: [],
    async query(request) {
      this.queries.push(request)
      if (shouldFail) throw new Error('publication read failed')
      return {
        schemaVersion: 'winwincode/v1',
        requestId: request.requestId,
        query: request.query,
        result: receiptDetail({ summary: { revision: 4, state: 'failed' } }),
        page: { hasMore: false, nextCursor: null },
      }
    },
  }
  const mounted = mountStrongFlowPage({
    root: rootElement,
    model,
    deliveryList: fakeDeliveryList([]),
    evidence: pageEvidenceOptions({ client }),
  })

  const sections = new Map(
    findAllByClass(rootElement, 'wwc-strongflow-review-detail-section')
      .map(section => [section.dataset.section, section]),
  )
  toggleOf(sections, 'publication').emit('click')
  await flush()

  const status = findByClass(rootElement, 'wwc-strongflow-review-detail-receipt-status')
  assert.match(status.textContent, /not available/u)
  // No invented traceability flags while the receipt is unreadable.
  assert.equal(findByClass(rootElement, 'wwc-strongflow-review-detail-receipt-retryable'), null)

  shouldFail = false
  findByClass(rootElement, 'wwc-strongflow-review-detail-receipt-refresh').emit('click')
  await flush()
  assert.equal(client.queries.length, 2)
  assert.match(
    findByClass(rootElement, 'wwc-strongflow-review-detail-receipt').textContent,
    /Retryable/u,
  )

  mounted.close()
  assert.deepEqual(rootElement.children, [])
})

test('the standalone panel mounts one labelled region and closes cleanly', () => {
  const document = new FakeDocument()
  const root = document.createElement('section')
  const model = new FakeStrongFlowViewModel(reviewState())
  const panel = mountStrongFlowReviewDetailPanel({
    document,
    root,
    model,
    copy: async () => {},
  })
  assert.equal(panel.root.getAttribute('aria-label'), 'Delivery review detail')
  panel.update()
  panel.close()
  assert.deepEqual(root.children, [])
})

function receiptDetail(overrides = {}) {
  const { summary: summaryOverrides, ...rest } = overrides
  const summary = {
    approvalAttentionItemId: 'attention:1',
    approvedAt: '2026-09-02T01:00:06.000Z',
    approvedBy: 'usr_00000000000000000000000002',
    candidateRef,
    deliveryId,
    deliverySpecId: 'spec:1',
    deliverySpecRevision: 3,
    deliveryVerdictId: 'verdict:1',
    id: 'pub_00000000000000000000000001',
    publicationSetSha256: publicationSetDigest,
    resourceRef: { kind: 'github_pull_request', number: 12, repository: 'owner/repo' },
    revision: 1,
    state: 'pending',
    target: {
      baseBranch: 'main',
      headBranch: 'winwincode/candidate-1',
      headRepository: 'owner/repo',
      provider: 'github',
      repository: 'owner/repo',
    },
    updatedAt: '2026-09-02T01:00:06.000Z',
    verdictStatus: 'pass',
    ...summaryOverrides,
  }
  return {
    cancellable: true,
    cancellation: null,
    history: [{
      cancellable: true,
      retryable: false,
      revision: 1,
      state: 'pending',
      stepStates: [{ kind: 'branch', state: 'pending' }],
      updatedAt: '2026-09-02T01:00:06.000Z',
    }],
    historyTruncated: false,
    kind: 'publication_detail',
    retryable: true,
    steps: [{
      kind: 'pull_request',
      outcomeCode: null,
      remoteWritePerformed: true,
      resourceRef: { kind: 'github_pull_request', number: 12, repository: 'owner/repo' },
      retryable: false,
      state: 'applying',
    }],
    summary,
    ...rest,
  }
}

function receiptClient() {
  return {
    queries: [],
    async query(request) {
      this.queries.push(request)
      assert.equal(request.query, 'publication.get')
      return {
        schemaVersion: 'winwincode/v1',
        requestId: request.requestId,
        query: request.query,
        result: receiptDetail({
          history: [
            {
              cancellable: true,
              retryable: false,
              revision: 1,
              state: 'pending',
              stepStates: [{ kind: 'branch', state: 'pending' }],
              updatedAt: '2026-09-02T01:00:06.000Z',
            },
            {
              cancellable: true,
              retryable: false,
              revision: 2,
              state: 'publishing',
              stepStates: [
                { kind: 'branch', state: 'succeeded' },
                { kind: 'pull_request', state: 'applying' },
              ],
              updatedAt: '2026-09-02T01:00:08.000Z',
            },
          ],
        }),
        page: { hasMore: false, nextCursor: null },
      }
    },
  }
}
function reviewState(overrides = {}) {
  const snapshot = projection()
  return {
    status: 'ready',
    realtime: 'subscribed',
    projection: snapshot,
    candidateFiles: {
      status: 'idle',
      items: [],
      hasMore: false,
      previewLimited: false,
      selectedPath: null,
      diff: {
        status: 'idle',
        path: null,
        content: '',
        loadedBytes: 0,
        totalBytes: null,
        hasMore: false,
        previewLimited: false,
        fileDiffSha256: null,
        unavailableReason: null,
        error: null,
      },
      error: null,
    },
    interaction: { status: 'idle', error: null },
    error: null,
    ...overrides,
  }
}

async function flush() {
  for (let index = 0; index < 8; index += 1) {
    await new Promise(resolveQueue => setImmediate(resolveQueue))
  }
}

function walk(node) {
  const found = [node]
  for (const child of node.children ?? []) found.push(...walk(child))
  return found
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
  readOnly = false
  required = false
  type = ''
  value = ''
  href = ''

  get textContent() {
    const own = this.#textContent
    return own + this.children.map(child => child.textContent).join('')
  }

  get childNodes() {
    return this.children
  }

  set textContent(value) {
    this.#textContent = String(value)
    this.replaceChildren()
  }

  #textContent = ''

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
    const event = { target: this, preventDefault() {}, ...values }
    let current = this
    while (current !== null) {
      for (const listener of current.listeners.get(name) ?? []) listener(event)
      current = current.parentNode
    }
  }

  closest(selector) {
    if (selector.startsWith('.') && this.className.split(' ').includes(selector.slice(1))) {
      return this
    }
    return this.parentNode?.closest?.(selector) ?? null
  }

  focus() {
    this.ownerDocument.activeElement = this
  }

  blur() {
    if (this.ownerDocument.activeElement === this) this.ownerDocument.activeElement = null
  }
}

class FakeDocument {
  activeElement = null

  createElement(tagName) {
    return new FakeElement(this, tagName)
  }
}


function toggleOf(sections, id) {
  const toggle = findByClass(sections.get(id), 'wwc-strongflow-review-detail-toggle')
  assert.notEqual(toggle, null, `missing ${id} toggle`)
  return toggle
}

function findByClass(node, className) {
  if (node.className === className) return node
  for (const child of node.children) {
    const match = findByClass(child, className)
    if (match !== null) return match
  }
  return null
}

function findAllByClass(node, className) {
  const found = []
  if (node.className === className) found.push(node)
  for (const child of node.children) found.push(...findAllByClass(child, className))
  return found
}

class FakeStrongFlowViewModel {
  constructor(initialState) {
    this.state = initialState
  }

  draftScope = '["review-detail-test-actor","review-detail-test-scope"]'
  calls = []
  listeners = new Set()

  subscribe(listener) {
    this.listeners.add(listener)
    listener(this.state)
    return () => {
      this.listeners.delete(listener)
    }
  }

  publish(next) {
    this.state = next
    for (const listener of this.listeners) listener(next)
  }

  async start() {
    this.calls.push(['start'])
  }
  async refresh() {
    this.calls.push(['refresh'])
  }
  async loadCandidateFiles() {
    this.calls.push(['loadCandidateFiles'])
  }
  async loadMoreCandidateFiles() {
    this.calls.push(['loadMoreCandidateFiles'])
  }
  async selectCandidateFile(path) {
    this.calls.push(['selectCandidateFile', path])
  }
  async loadMoreCandidateDiff() {
    this.calls.push(['loadMoreCandidateDiff'])
  }
  async decideSolutionReview(input) {
    this.calls.push(['decideSolutionReview', input])
  }
  async approveTaskBreakdown() {
    this.calls.push(['approveTaskBreakdown'])
  }
  async resolveAttention(input) {
    this.calls.push(['resolveAttention', input])
  }
  async loadStageRunRuntime() {
    return null
  }
  async loadStageRunCandidates() {
    return []
  }
  async loadDeliveryCandidates() {
    return []
  }
  async loadCandidateHistoricalReview() {
    return null
  }
  async submitVerdict() {
    this.calls.push(['submitVerdict'])
  }
  async advanceDelivery() {
    this.calls.push(['advanceDelivery'])
  }
  cancelPending() {
    this.calls.push(['cancelPending'])
  }
  reconnect() {
    this.calls.push(['reconnect'])
  }
  close() {
    this.calls.push(['close'])
  }
}

class FakeDeliveryListModel {
  constructor(visible) {
    this.state = {
      status: 'ready',
      filters: { search: '', status: null, attentionOnly: false, order: 'recent' },
      visible,
      loadedCount: visible.length,
      hasMore: false,
      loadingMore: false,
      moreFailure: null,
      error: null,
      advance: { deliveryId: null, failure: null },
    }
  }

  calls = []
  listener = null

  subscribe(listener) {
    this.listener = listener
    listener(this.state)
    return () => {
      this.listener = null
    }
  }

  async refresh() {
    this.calls.push(['refresh'])
  }
  async loadMore() {
    this.calls.push(['loadMore'])
  }
  setSearch(value) {
    this.calls.push(['setSearch', value])
  }
  async setStatusFilter(value) {
    this.calls.push(['setStatusFilter', value])
  }
  setAttentionOnly(value) {
    this.calls.push(['setAttentionOnly', value])
  }
  setOrder(value) {
    this.calls.push(['setOrder', value])
  }
  async advanceDelivery(id, revision) {
    this.calls.push(['advanceDelivery', id, revision])
  }
  close() {
    this.calls.push(['close'])
  }
}

function fakeDeliveryList(visible) {
  return new FakeDeliveryListModel(visible)
}

function pageEvidenceDeepLink() {
  const state = { hash: `#/strongflow?delivery=${deliveryId}` }
  const link = {
    get route() {
      const parameters = new URLSearchParams(state.hash.slice(state.hash.indexOf('?') + 1))
      const tab = parameters.get('tab')
      return {
        tab: tab === 'preview' || tab === 'tests' || tab === 'logs' ? tab : 'evidence',
        evidenceId: parameters.get('evidence'),
      }
    },
    onRouteChange(route) {
      const parameters = new URLSearchParams(state.hash.slice(state.hash.indexOf('?') + 1))
      parameters.set('tab', route.tab)
      if (route.evidenceId === null) parameters.delete('evidence')
      else parameters.set('evidence', route.evidenceId)
      state.hash = `#/strongflow?${parameters.toString()}`
    },
    state,
  }
  return link
}

function pageEvidenceOptions(overrides = {}) {
  const link = pageEvidenceDeepLink()
  return {
    client: receiptClient(),
    actor,
    scope,
    nextRequestId: () => 'req_00000000000000000000000001',
    route: link.route,
    onRouteChange: link.onRouteChange,
    ...overrides,
  }
}
