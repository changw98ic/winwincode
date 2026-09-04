// SPDX-License-Identifier: Apache-2.0
//
// UI-405: baseline→Candidate and Candidate→Candidate comparison.  The Diff
// model owns the comparison sources and every rejection, the typed StrongFlow
// route owns the shareable `compareFrom`/`compareTo` parameters, and the
// mounted panel renders exactly one stable summary per Delivery.

import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { resolve } from 'node:path'
import { pathToFileURL } from 'node:url'
import test from 'node:test'

const root = resolve(import.meta.dirname, '..')
const compiler = spawnSync('corepack', [
  'pnpm',
  'exec',
  'tsc',
  '-p',
  'apps/client/tsconfig.strongflow-comparison-tests.json',
  '--pretty',
  'false',
  '--incremental',
  'false',
], { cwd: root, encoding: 'utf8' })
assert.equal(
  compiler.status,
  0,
  `StrongFlow Candidate comparison modules did not compile:\n${compiler.stdout}${compiler.stderr}`,
)

const cache = resolve(root, '.cache/strongflow-comparison-tests')
const modelModule = await import(`${pathToFileURL(resolve(
  cache,
  'strongflow-diff-model.js',
)).href}`)
const routeModule = await import(`${pathToFileURL(resolve(
  cache,
  'strongflow-route.js',
)).href}`)
const comparisonModule = await import(`${pathToFileURL(resolve(
  cache,
  'strongflow-candidate-comparison.js',
)).href}`)

const {
  candidateComparisonBaselineSide,
  candidateComparisonChoices,
  candidateComparisonDefaultRequest,
  compareCandidateReviews,
  formatCandidateComparisonIdentity,
  formatCandidateComparisonRequest,
  parseCandidateComparisonIdentity,
  parseCandidateComparisonRequest,
  resolveCandidateComparison,
} = modelModule
const { parseStrongFlowRouteHash, strongFlowRouteHash } = routeModule
const {
  mountStrongFlowCandidateComparison,
  strongFlowCandidateComparisonSide,
} = comparisonModule

const DIGEST_ONE = `sha256:${'1'.repeat(64)}`
const DIGEST_TWO = `sha256:${'2'.repeat(64)}`
const REF_ONE = 'refs/winwincode/candidate/attempt-1'
const REF_TWO = 'refs/winwincode/candidate/attempt-2'
const REWORK_DIGEST = `sha256:${'9'.repeat(64)}`
const REWORK_REF = 'refs/winwincode/candidate/attempt-0'
const DELIVERY_ID = 'dlv_00000000000000000000000001'
const STAGE_RUN_ID = 'run_00000000000000000000000001'

function candidateSummary(overrides = {}) {
  return {
    candidateCommitId: 'c'.repeat(40),
    candidateRef: REF_ONE,
    candidateTreeId: 't'.repeat(40),
    deliverySpecId: 'spec-candidate-compare',
    deliverySpecRevision: 4,
    diffSha256: DIGEST_ONE,
    frozenAt: '2026-01-01T00:00:00Z',
    producerSessionBindingId: 'binding-1',
    producerStageRunId: STAGE_RUN_ID,
    ...overrides,
  }
}

function changedFile(overrides = {}) {
  return {
    additions: 4,
    deletions: 1,
    binary: false,
    encoding: 'utf-8',
    oldPath: null,
    path: 'src/app.ts',
    status: 'modified',
    ...overrides,
  }
}

function evidence(id, overrides = {}) {
  return {
    candidateRef: REF_ONE,
    createdAt: '2026-01-01T00:00:00Z',
    deliverySpecId: 'spec-candidate-compare',
    deliverySpecRevision: 4,
    id,
    sessionBindingId: 'binding-1',
    sourceRef: `test://evidence/${id}`,
    stageRunId: STAGE_RUN_ID,
    type: 'test',
    ...overrides,
  }
}

function criterion(id, result) {
  return {
    criterionId: id,
    evaluatedAt: '2026-01-01T00:00:00Z',
    evidenceRefs: [],
    explanation: 'Automated verification outcome.',
    resultId: result,
    verdict: 'pass',
  }
}

function verdict(overrides = {}) {
  return {
    candidateRef: REF_ONE,
    criteria: [criterion('criterion-tests', 'result-1')],
    deliverySpecId: 'spec-candidate-compare',
    deliverySpecRevision: 4,
    id: 'verdict-1',
    producedAt: '2026-01-01T00:00:00Z',
    status: 'pass',
    unresolvedFindings: [],
    ...overrides,
  }
}

function historicalReview(overrides = {}) {
  return {
    availability: 'available',
    candidate: candidateSummary(),
    currentAuthorization: false,
    displayOnly: true,
    evidence: [evidence('evd_00000000000000000000000001')],
    firstSeenDeliveryRevision: 1,
    kind: 'candidate_historical_review',
    lastSeenDeliveryRevision: 7,
    readCursor: {
      deliveryId: DELIVERY_ID,
      deliveryRevision: 7,
      eventCursor: '0',
      publicationRevision: 0,
      runtimeAcceptedSequence: 0,
      runtimeLedgerRevision: 0,
    },
    reviewDeliveryRevision: null,
    verdict: verdict(),
    ...overrides,
  }
}

function historyItem(overrides = {}) {
  return {
    availability: 'available',
    candidate: candidateSummary(),
    firstSeenDeliveryRevision: 1,
    isCurrentAtReadCursor: false,
    lastSeenDeliveryRevision: 7,
    reviewDeliveryRevision: null,
    ...overrides,
  }
}

function choice(overrides = {}) {
  return {
    candidate: candidateSummary(overrides.candidate ?? {}),
    availability: 'available',
    isCurrent: false,
    ...overrides,
  }
}

function context(overrides = {}) {
  return {
    deliverySpecId: 'spec-candidate-compare',
    deliverySpecRevision: 4,
    choices: [],
    ...overrides,
  }
}

function identity(ref, digest) {
  return { candidateRef: ref, diffSha256: digest }
}

const afterOne = identity(REF_ONE, DIGEST_ONE)

// ── shareable comparison link tokens ───────────────────────────────────────

test('comparison identities round-trip through one bounded link token', () => {
  const token = formatCandidateComparisonIdentity(afterOne)
  assert.equal(token, `${REF_ONE}~${DIGEST_ONE}`)
  assert.deepEqual(parseCandidateComparisonIdentity(token), afterOne)
})

test('comparison link tokens reject separators, empty halves, and non-string values', () => {
  assert.equal(parseCandidateComparisonIdentity(REF_ONE), null)
  assert.equal(parseCandidateComparisonIdentity(`~${DIGEST_ONE}`), null)
  assert.equal(parseCandidateComparisonIdentity(`${REF_ONE}~`), null)
  assert.equal(parseCandidateComparisonIdentity(''), null)
  assert.equal(parseCandidateComparisonIdentity(undefined), null)
  assert.equal(parseCandidateComparisonIdentity(null), null)
  assert.equal(parseCandidateComparisonIdentity(42), null)
  // A second separator can never smuggle another value into one token.
  assert.equal(
    parseCandidateComparisonIdentity(`${REF_ONE}~${DIGEST_ONE}~extra`),
    null,
  )
})

test('comparison requests format into two route values the parser accepts', () => {
  const request = { before: null, after: afterOne }
  const formatted = formatCandidateComparisonRequest(request)
  assert.deepEqual(formatted, { before: null, after: `${REF_ONE}~${DIGEST_ONE}` })
  assert.deepEqual(
    parseCandidateComparisonRequest(formatted.before, formatted.after),
    { status: 'requested', request },
  )
  const pair = { before: identity(REF_TWO, DIGEST_TWO), after: afterOne }
  const pairValues = formatCandidateComparisonRequest(pair)
  assert.deepEqual(
    parseCandidateComparisonRequest(pairValues.before, pairValues.after),
    { status: 'requested', request: pair },
  )
})

test('incomplete or malformed comparison links fail closed as invalid', () => {
  assert.deepEqual(parseCandidateComparisonRequest(null, null), { status: 'none' })
  assert.equal(
    parseCandidateComparisonRequest(`${REF_ONE}~${DIGEST_ONE}`, null).status,
    'invalid',
  )
  assert.equal(parseCandidateComparisonRequest(null, REF_ONE).status, 'invalid')
  assert.equal(
    parseCandidateComparisonRequest(null, `${REF_ONE}~${DIGEST_ONE}~extra`).status,
    'invalid',
  )
  assert.equal(parseCandidateComparisonRequest('baseline~', `${REF_ONE}~${DIGEST_ONE}`).status, 'invalid')
  assert.deepEqual(
    parseCandidateComparisonRequest('baseline', `${REF_ONE}~${DIGEST_ONE}`).status,
    'invalid',
  )
})

// ── selection resolution and rejections ────────────────────────────────────

test('frozen history items become comparison choices in Delivery order', () => {
  assert.deepEqual(
    candidateComparisonChoices([
      historyItem({ candidate: candidateSummary({ candidateRef: REF_TWO, diffSha256: DIGEST_TWO }) }),
      historyItem({ isCurrentAtReadCursor: true }),
    ]),
    [
      choice({ candidate: candidateSummary({ candidateRef: REF_TWO, diffSha256: DIGEST_TWO }) }),
      choice({ isCurrent: true }),
    ],
  )
})

test('a baseline comparison resolves against the Delivery base revision', () => {
  const delivery = context({ choices: [choice({ isCurrent: true })] })
  assert.deepEqual(resolveCandidateComparison(delivery, { before: null, after: afterOne }), {
    status: 'resolved',
    before: null,
    after: choice({ isCurrent: true }),
  })
})

test('two Candidates of one Delivery resolve in frozen history order', () => {
  const before = choice({
    candidate: candidateSummary({ candidateRef: REF_TWO, diffSha256: DIGEST_TWO }),
  })
  const current = choice({ isCurrent: true })
  const delivery = context({ choices: [before, current] })
  assert.deepEqual(
    resolveCandidateComparison(delivery, {
      before: identity(REF_TWO, DIGEST_TWO),
      after: afterOne,
    }),
    { status: 'resolved', before, after: current },
  )
})

test('a Candidate outside the current Delivery history is rejected as missing', () => {
  const delivery = context({ choices: [choice({ isCurrent: true })] })
  const resolution = resolveCandidateComparison(delivery, {
    before: null,
    after: identity('refs/winwincode/candidate/other', DIGEST_ONE),
  })
  assert.equal(resolution.status, 'rejected')
  assert.equal(resolution.rejection.reason, 'missing-candidate')
  assert.equal(resolution.rejection.candidateRef, 'refs/winwincode/candidate/other')
  assert.ok(resolution.rejection.message.length > 0)
  const missingBefore = resolveCandidateComparison(delivery, {
    before: identity('refs/winwincode/candidate/other', DIGEST_ONE),
    after: afterOne,
  })
  assert.equal(missingBefore.status, 'rejected')
  assert.equal(missingBefore.rejection.reason, 'missing-candidate')
})

test('a link cut from an older read cursor is rejected as stale', () => {
  const delivery = context({
    choices: [
      choice({
        candidate: candidateSummary({ candidateRef: REF_TWO, diffSha256: DIGEST_TWO }),
      }),
      choice({ isCurrent: true }),
    ],
  })
  const stale = resolveCandidateComparison(delivery, {
    before: null,
    after: identity(REF_ONE, DIGEST_TWO),
  })
  assert.equal(stale.status, 'rejected')
  assert.equal(stale.rejection.reason, 'stale-candidate')
  const staleBefore = resolveCandidateComparison(delivery, {
    before: identity(REF_TWO, DIGEST_ONE),
    after: afterOne,
  })
  assert.equal(staleBefore.status, 'rejected')
  assert.equal(staleBefore.rejection.reason, 'stale-candidate')
})

test('a Candidate frozen under another Delivery Spec is rejected', () => {
  const delivery = context({
    choices: [
      choice({
        candidate: candidateSummary({ deliverySpecRevision: 3 }),
        isCurrent: true,
      }),
    ],
  })
  const resolution = resolveCandidateComparison(delivery, {
    before: null,
    after: identity(REF_ONE, DIGEST_ONE),
  })
  assert.equal(resolution.status, 'rejected')
  assert.equal(resolution.rejection.reason, 'foreign-delivery')
  const foreignBefore = resolveCandidateComparison(delivery, {
    before: identity(REF_ONE, DIGEST_ONE),
    after: identity(REF_ONE, DIGEST_ONE),
  })
  assert.equal(foreignBefore.rejection.reason, 'same-candidate')
})

test('comparing one Candidate with itself is rejected before any lookup', () => {
  const resolution = resolveCandidateComparison(context(), {
    before: afterOne,
    after: afterOne,
  })
  assert.equal(resolution.status, 'rejected')
  assert.equal(resolution.rejection.reason, 'same-candidate')
})

// ── default comparison requests ────────────────────────────────────────────

test('returning from bounded rework defaults to the rework before and after Candidates', () => {
  const delivery = context({
    choices: [
      choice({
        candidate: candidateSummary({ candidateRef: REWORK_REF, diffSha256: REWORK_DIGEST }),
      }),
      choice({
        candidate: candidateSummary({ candidateRef: REF_TWO, diffSha256: DIGEST_TWO }),
      }),
      choice({ isCurrent: true }),
    ],
  })
  assert.deepEqual(
    candidateComparisonDefaultRequest(delivery, {
      reworkBaselineDigest: REWORK_DIGEST,
      reworkStage: false,
    }),
    { before: identity(REWORK_REF, REWORK_DIGEST), after: afterOne },
  )
})

test('a rework stage defaults to the Candidate frozen before the current one', () => {
  const delivery = context({
    choices: [
      choice({
        candidate: candidateSummary({ candidateRef: REF_TWO, diffSha256: DIGEST_TWO }),
      }),
      choice({ isCurrent: true }),
    ],
  })
  assert.deepEqual(
    candidateComparisonDefaultRequest(delivery, {
      reworkBaselineDigest: null,
      reworkStage: true,
    }),
    { before: identity(REF_TWO, DIGEST_TWO), after: afterOne },
  )
})

test('without a rework the default comparison stays baseline to current Candidate', () => {
  const delivery = context({ choices: [choice({ isCurrent: true })] })
  assert.deepEqual(
    candidateComparisonDefaultRequest(delivery, {
      reworkBaselineDigest: null,
      reworkStage: false,
    }),
    { before: null, after: afterOne },
  )
  // A recorded rework digest that no longer resolves never invents a pair.
  assert.deepEqual(
    candidateComparisonDefaultRequest(delivery, {
      reworkBaselineDigest: REWORK_DIGEST,
      reworkStage: false,
    }),
    { before: null, after: afterOne },
  )
})

test('a Delivery without a frozen Candidate has no default comparison', () => {
  assert.equal(
    candidateComparisonDefaultRequest(context(), {
      reworkBaselineDigest: null,
      reworkStage: true,
    }),
    null,
  )
})

// ── comparison result ──────────────────────────────────────────────────────

test('baseline comparison sorts Candidate paths by their changed-file status', () => {
  const result = compareCandidateReviews(
    candidateComparisonBaselineSide(),
    {
      role: 'candidate',
      candidate: candidateSummary(),
      availability: 'available',
      files: [
        changedFile({ path: 'src/app.ts' }),
        changedFile({ path: 'docs/new.md', status: 'added', additions: 12, deletions: 0 }),
        changedFile({ path: 'src/legacy.ts', status: 'deleted', additions: 0, deletions: 9 }),
        changedFile({
          path: 'src/renamed.ts',
          status: 'renamed',
          oldPath: 'src/old-name.ts',
          additions: 2,
          deletions: 1,
        }),
      ],
      evidenceIds: ['evd_00000000000000000000000001'],
      verdict: verdict(),
    },
  )
  assert.equal(result.diffChanged, true)
  assert.equal(result.files.known, true)
  assert.deepEqual(result.files.added, ['docs/new.md', 'src/renamed.ts'])
  assert.deepEqual(result.files.removed, ['src/legacy.ts'])
  assert.deepEqual(result.files.changed, ['src/app.ts'])
  assert.deepEqual(result.files.changes.map(change => change.path), [
    'docs/new.md',
    'src/renamed.ts',
    'src/legacy.ts',
    'src/app.ts',
  ])
  assert.deepEqual(result.files.changes.map(change => change.kind), [
    'added',
    'added',
    'removed',
    'changed',
  ])
  assert.equal(result.files.beforeAdditions, 0)
  assert.equal(result.files.beforeDeletions, 0)
  assert.equal(result.files.additions, 18)
  assert.equal(result.files.deletions, 11)
  assert.deepEqual(result.evidence.added, ['evd_00000000000000000000000001'])
  assert.deepEqual(result.evidence.removed, [])
  assert.equal(result.evidence.unchangedCount, 0)
  assert.equal(result.verdict.beforeStatus, null)
  assert.equal(result.verdict.afterStatus, 'pass')
  assert.equal(result.verdict.changed, true)
  assert.deepEqual(result.verdict.criteria, [{
    criterionId: 'criterion-tests',
    before: null,
    after: 'pass',
  }])
  assert.equal(result.changed, true)
})

test('Candidate comparison reports added, removed, and changed paths plus review deltas', () => {
  const result = compareCandidateReviews(
    {
      role: 'candidate',
      candidate: candidateSummary({ candidateRef: REF_TWO, diffSha256: DIGEST_TWO }),
      availability: 'available',
      files: [
        changedFile({ path: 'src/app.ts' }),
        changedFile({ path: 'src/dropped.ts', status: 'deleted', additions: 0, deletions: 4 }),
        changedFile({ path: 'src/same.ts', status: 'added', additions: 2, deletions: 0 }),
      ],
      evidenceIds: ['evd_00000000000000000000000001', 'evd_00000000000000000000000002'],
      verdict: verdict({
        status: 'fail',
        criteria: [criterion('criterion-tests', 'result-0')],
      }),
    },
    {
      role: 'candidate',
      candidate: candidateSummary(),
      availability: 'available',
      files: [
        changedFile({ path: 'src/app.ts', additions: 6, deletions: 3 }),
        changedFile({ path: 'src/extra.ts', status: 'added', additions: 5, deletions: 0 }),
        changedFile({ path: 'src/same.ts', status: 'added', additions: 2, deletions: 0 }),
      ],
      evidenceIds: ['evd_00000000000000000000000002', 'evd_00000000000000000000000003'],
      verdict: verdict({
        criteria: [
          criterion('criterion-tests', 'result-1'),
          criterion('criterion-review', 'result-2'),
        ],
      }),
    },
  )
  assert.equal(result.diffChanged, true)
  assert.equal(result.files.known, true)
  assert.deepEqual(result.files.added, ['src/extra.ts'])
  assert.deepEqual(result.files.removed, ['src/dropped.ts'])
  assert.deepEqual(result.files.changed, ['src/app.ts'])
  assert.equal(result.files.beforeAdditions, 6)
  assert.equal(result.files.beforeDeletions, 5)
  assert.equal(result.files.additions, 13)
  assert.equal(result.files.deletions, 3)
  assert.deepEqual(result.evidence.added, ['evd_00000000000000000000000003'])
  assert.deepEqual(result.evidence.removed, ['evd_00000000000000000000000001'])
  assert.equal(result.evidence.unchangedCount, 1)
  assert.equal(result.verdict.beforeStatus, 'fail')
  assert.equal(result.verdict.afterStatus, 'pass')
  assert.equal(result.verdict.changed, true)
  // Only the Verdict *changes* are listed; the unchanged criterion is not.
  assert.deepEqual(result.verdict.criteria, [
    { criterionId: 'criterion-review', before: null, after: 'pass' },
  ])
  assert.equal(result.changed, true)
})

test('an unchanged Candidate pair still reports one stable comparison', () => {
  const files = [changedFile(), changedFile({ path: 'src/lib.ts' })]
  const evidenceIds = ['evd_00000000000000000000000001']
  const sharedVerdict = verdict()
  const result = compareCandidateReviews(
    {
      // A re-frozen Candidate that shares one tree with its predecessor.
      role: 'candidate',
      candidate: candidateSummary({ candidateRef: REF_TWO, diffSha256: DIGEST_ONE }),
      availability: 'available',
      files,
      evidenceIds,
      verdict: sharedVerdict,
    },
    {
      role: 'candidate',
      candidate: candidateSummary(),
      availability: 'available',
      files: [changedFile(), changedFile({ path: 'src/lib.ts' })],
      evidenceIds,
      verdict: sharedVerdict,
    },
  )
  assert.equal(result.diffChanged, false)
  assert.equal(result.files.known, true)
  assert.deepEqual(result.files.added, [])
  assert.deepEqual(result.files.removed, [])
  assert.deepEqual(result.files.changed, [])
  assert.deepEqual(result.evidence.added, [])
  assert.deepEqual(result.evidence.removed, [])
  assert.equal(result.verdict.changed, false)
  assert.deepEqual(result.verdict.criteria, [])
  assert.equal(result.changed, false)
})

test('a released Candidate keeps its review facts without inventing an inventory', () => {
  const result = compareCandidateReviews(
    {
      role: 'candidate',
      candidate: candidateSummary({ candidateRef: REF_TWO, diffSha256: DIGEST_TWO }),
      availability: 'released',
      files: null,
      evidenceIds: ['evd_00000000000000000000000001'],
      verdict: null,
    },
    {
      role: 'candidate',
      candidate: candidateSummary(),
      availability: 'available',
      files: [changedFile()],
      evidenceIds: [],
      verdict: verdict(),
    },
  )
  assert.equal(result.files.known, false)
  assert.deepEqual(result.files.changes, [])
  assert.deepEqual(result.files.added, [])
  assert.equal(result.files.additions, null)
  assert.equal(result.files.deletions, null)
  assert.deepEqual(result.evidence.removed, ['evd_00000000000000000000000001'])
  assert.equal(result.changed, true)
})

// ── sides built from the delivered review reads ────────────────────────────

test('comparison sides carry baseline emptiness and Candidate review facts', () => {
  assert.deepEqual(
    strongFlowCandidateComparisonSide(null, historicalReview(), null),
    candidateComparisonBaselineSide(),
  )
  const current = choice({ isCurrent: true })
  const review = historicalReview({
    evidence: [evidence('evd_00000000000000000000000002')],
    verdict: null,
  })
  assert.deepEqual(strongFlowCandidateComparisonSide(current, review, null), {
    role: 'candidate',
    candidate: current.candidate,
    availability: 'available',
    files: null,
    evidenceIds: ['evd_00000000000000000000000002'],
    verdict: null,
  })
  const files = [changedFile()]
  // Only the current Candidate owns a readable changed-file inventory.
  assert.equal(
    strongFlowCandidateComparisonSide(current, review, {
      candidateRef: REF_TWO,
      files,
      known: true,
    }).files,
    null,
  )
  assert.deepEqual(
    strongFlowCandidateComparisonSide(current, review, {
      candidateRef: REF_ONE,
      files,
      known: true,
    }).files,
    files,
  )
  assert.deepEqual(
    strongFlowCandidateComparisonSide(current, review, {
      candidateRef: REF_ONE,
      files,
      known: false,
    }).files,
    null,
  )
  // A missing review keeps the Candidate identity without inventing facts.
  assert.deepEqual(strongFlowCandidateComparisonSide(current, null, null), {
    role: 'candidate',
    candidate: current.candidate,
    availability: 'available',
    files: null,
    evidenceIds: [],
    verdict: null,
  })
})

// ── typed route ────────────────────────────────────────────────────────────

function baseRoute(overrides = {}) {
  return {
    deliveryId: 'dlv_00000000000000000000000001',
    productSessionId: 'psn_00000000000000000000000001',
    stageRunId: STAGE_RUN_ID,
    candidatePath: null,
    candidateView: 'unified',
    comparison: { status: 'none' },
    evidenceTab: 'evidence',
    evidenceId: null,
    ...overrides,
  }
}

test('the canonical StrongFlow route carries one shareable comparison', () => {
  const comparison = {
    status: 'requested',
    request: { before: identity(REF_TWO, DIGEST_TWO), after: afterOne },
  }
  const hash = strongFlowRouteHash(baseRoute({ comparison }))
  const parameters = new URLSearchParams(hash.slice(hash.indexOf('?') + 1))
  assert.equal(parameters.get('compareFrom'), `${REF_TWO}~${DIGEST_TWO}`)
  assert.equal(parameters.get('compareTo'), `${REF_ONE}~${DIGEST_ONE}`)
  assert.deepEqual(parseStrongFlowRouteHash(hash).comparison, comparison)

  const baselineHash = strongFlowRouteHash(
    baseRoute({
      comparison: {
        status: 'requested',
        request: { before: null, after: afterOne },
      },
    }),
  )
  assert.equal(baselineHash.includes('compareFrom='), false, baselineHash)
  assert.deepEqual(parseStrongFlowRouteHash(baselineHash).comparison, {
    status: 'requested',
    request: { before: null, after: afterOne },
  })
  // `compareFrom=baseline` names the same Delivery base revision.
  const baselineKeyword = parseStrongFlowRouteHash(
    `#/strongflow?compareTo=${encodeURIComponent(`${REF_ONE}~${DIGEST_ONE}`)}`
      + '&compareFrom=baseline',
  )
  assert.deepEqual(baselineKeyword.comparison, {
    status: 'requested',
    request: { before: null, after: afterOne },
  })
})

test('a StrongFlow route without comparison parameters stays exactly as before', () => {
  assert.deepEqual(parseStrongFlowRouteHash(strongFlowRouteHash(baseRoute())), baseRoute())
  assert.equal(strongFlowRouteHash(baseRoute()).includes('compare'), false)
})

test('a rejected comparison link is never written back to the canonical route', () => {
  const stale = parseStrongFlowRouteHash(
    `#/strongflow?compareTo=${encodeURIComponent(`${REF_ONE}~${DIGEST_ONE}`)}`
      + `&compareFrom=${encodeURIComponent(REF_ONE)}`,
  )
  assert.equal(stale.comparison.status, 'invalid')
  const rewritten = strongFlowRouteHash(stale)
  assert.equal(rewritten.includes('compare'), false, rewritten)
  assert.equal(parseStrongFlowRouteHash(rewritten).comparison.status, 'none')
})

test('comparison links never carry changed-file paths or Diff bytes', () => {
  const comparisonHash = `#/strongflow?compareTo=${encodeURIComponent(`${REF_ONE}~${DIGEST_ONE}`)}`
  assert.ok(!comparisonHash.includes('src/'), comparisonHash)
  assert.ok(!comparisonHash.includes('.ts'), comparisonHash)
  assert.ok(!comparisonHash.includes('blob'), comparisonHash)
  const hash = strongFlowRouteHash(
    baseRoute({
      candidatePath: 'src/internal-layout.ts',
      comparison: {
        status: 'requested',
        request: { before: null, after: afterOne },
      },
    }),
  )
  assert.ok(hash.includes('file=src%2Finternal-layout.ts'), hash)
})

// ── mounted panel ──────────────────────────────────────────────────────────

class FakeElement {
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
    this.value = ''
    this.parentNode = null
    this.#textContent = ''
  }

  #textContent

  get textContent() {
    return this.#textContent + this.children.map(child => child.textContent).join('')
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

  setAttribute(name, value) { this.attributes.set(name, String(value)) }
  getAttribute(name) { return this.attributes.get(name) ?? null }
  removeAttribute(name) { this.attributes.delete(name) }

  addEventListener(name, listener) {
    const listeners = this.listeners.get(name) ?? []
    listeners.push(listener)
    this.listeners.set(name, listeners)
  }

  removeEventListener(name, listener) {
    this.listeners.set(name, (this.listeners.get(name) ?? []).filter(item => item !== listener))
  }

  emit(name) {
    const event = { target: this, preventDefault() {} }
    for (const listener of this.listeners.get(name) ?? []) listener(event)
  }
}

class FakeDocument {
  activeElement = null
  createElement(tagName) { return new FakeElement(this, tagName) }
}

function findAllByClass(node, className, matches = []) {
  if (node.className === className) matches.push(node)
  for (const child of node.children) findAllByClass(child, className, matches)
  return matches
}

function findByClass(node, className) {
  return findAllByClass(node, className)[0] ?? null
}

function projection(overrides = {}) {
  const { delivery: deliveryOverrides, ...rest } = overrides
  return {
    delivery: {
      attention: [],
      currentCandidate: candidateSummary(),
      deliveryId: DELIVERY_ID,
      deliveryRevision: 7,
      diagramExecution: null,
      evidence: [],
      kind: 'delivery_detail',
      ownership: {},
      publication: null,
      readCursor: {},
      requirements: {
        deliverySpecId: 'spec-candidate-compare',
        deliverySpecRevision: 4,
      },
      solutionReview: null,
      stages: [],
      status: 'verifying',
      tasks: [],
      verdict: null,
      ...deliveryOverrides,
    },
    stage: { id: STAGE_RUN_ID },
    currentCandidate: candidateSummary(),
    ...rest,
  }
}

function candidateFilesState(overrides = {}) {
  return {
    status: 'ready',
    items: [changedFile(), changedFile({ path: 'src/lib.ts' })],
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
    ...overrides,
  }
}

const DEFAULT_PROJECTION = projection()

const DEFAULT_HISTORY = [
  historyItem({ candidate: candidateSummary({ candidateRef: REF_TWO, diffSha256: DIGEST_TWO }) }),
  historyItem({ isCurrentAtReadCursor: true }),
]

function mountComparison({
  projection: projectionValue = DEFAULT_PROJECTION,
  requested = { status: 'none' },
  reworkBaselineDigest = null,
  history = DEFAULT_HISTORY,
  reviews = null,
  onSelectionChange = null,
} = {}) {
  const document = new FakeDocument()
  const loadCalls = []
  const view = mountStrongFlowCandidateComparison({
    document,
    limits: {
      deliveries: 50,
      tasks: 100,
      stages: 50,
      attention: 50,
      evidence: 100,
      runtimeSessions: 50,
      graphNodes: 100,
      graphEdges: 200,
      activities: 100,
    },
    loadCandidates() {
      loadCalls.push(['candidates'])
      return Promise.resolve(history)
    },
    loadReview(candidate) {
      loadCalls.push(['review', candidate.candidateRef])
      return Promise.resolve(reviews?.[candidate.candidateRef] ?? null)
    },
    ...(onSelectionChange === null ? {} : { onSelectionChange }),
  })
  view.update({
    projection: projectionValue,
    candidateFiles: candidateFilesState(),
    requested,
    reworkBaselineDigest,
  })
  return { document, view, loadCalls }
}

function flush() {
  return new Promise(resolveQueue => setImmediate(resolveQueue))
}

test('the panel offers the Delivery baseline and exactly its own frozen Candidates', async () => {
  const { view } = mountComparison()
  await flush()
  const from = findByClass(view.root, 'wwc-strongflow-candidate-comparison-from')
  const to = findByClass(view.root, 'wwc-strongflow-candidate-comparison-to')
  assert.deepEqual([...from.children].map(option => option.value), [
    'baseline',
    REF_TWO,
    REF_ONE,
  ])
  assert.deepEqual([...to.children].map(option => option.value), [REF_TWO, REF_ONE])
  // Without a rework the default comparison is the Delivery baseline.
  assert.equal(from.value, 'baseline')
  assert.equal(to.value, REF_ONE)
  assert.equal(findByClass(view.root, 'wwc-strongflow-candidate-comparison-alert').hidden, true)
})

test('returning from bounded rework defaults to the rework pair and renders its summary', async () => {
  const reworkReview = historicalReview({
    candidate: candidateSummary({ candidateRef: REWORK_REF, diffSha256: REWORK_DIGEST }),
    evidence: [],
    verdict: null,
  })
  const { view, loadCalls } = mountComparison({
    reworkBaselineDigest: REWORK_DIGEST,
    history: [
      historyItem({
        candidate: candidateSummary({ candidateRef: REWORK_REF, diffSha256: REWORK_DIGEST }),
      }),
      historyItem({ isCurrentAtReadCursor: true }),
    ],
    reviews: { [REF_ONE]: historicalReview(), [REWORK_REF]: reworkReview },
  })
  await flush()
  const from = findByClass(view.root, 'wwc-strongflow-candidate-comparison-from')
  const to = findByClass(view.root, 'wwc-strongflow-candidate-comparison-to')
  assert.equal(from.value, REWORK_REF)
  assert.equal(to.value, REF_ONE)
  assert.ok(
    loadCalls.some(([kind, ref]) => kind === 'review' && ref === REWORK_REF),
    JSON.stringify(loadCalls),
  )
  const files = findByClass(view.root, 'wwc-strongflow-candidate-comparison-files')
  assert.equal(files.dataset.known, 'false')
  assert.equal(files.dataset.added, '0')
  const verdictLine = findByClass(view.root, 'wwc-strongflow-candidate-comparison-verdict')
  assert.equal(verdictLine.dataset.changed, 'true')
  assert.ok(verdictLine.textContent.includes('pass'), verdictLine.textContent)
  // One reworked Delivery reads exactly the two compared Candidates, never more.
  assert.equal(loadCalls.filter(([kind]) => kind === 'review').length, 2)
})

test('a stale comparison link is announced as an alert and renders no summary', async () => {
  const { view } = mountComparison({
    requested: {
      status: 'requested',
      request: { before: null, after: identity(REF_ONE, DIGEST_TWO) },
    },
  })
  await flush()
  const alert = findByClass(view.root, 'wwc-strongflow-candidate-comparison-alert')
  assert.equal(alert.getAttribute('role'), 'alert')
  assert.equal(alert.hidden, false)
  assert.equal(alert.textContent.length > 0, true)
  assert.equal(
    findByClass(view.root, 'wwc-strongflow-candidate-comparison-summary').hidden,
    true,
  )
  // The selectors recover to the Delivery default instead of the dead link.
  const from = findByClass(view.root, 'wwc-strongflow-candidate-comparison-from')
  assert.equal(from.hidden, false)
  assert.equal(from.value, 'baseline')
  assert.equal(
    findByClass(view.root, 'wwc-strongflow-candidate-comparison-to').value,
    REF_ONE,
  )
})

test('an invalid comparison link is announced without echoing its values', async () => {
  const { view } = mountComparison({ requested: { status: 'invalid' } })
  await flush()
  const alert = findByClass(view.root, 'wwc-strongflow-candidate-comparison-alert')
  assert.equal(alert.hidden, false)
  assert.equal(alert.textContent.includes('~'), false, alert.textContent)
  assert.equal(
    findByClass(view.root, 'wwc-strongflow-candidate-comparison-summary').hidden,
    true,
  )
})

test('choosing two Candidates publishes one shareable request and renders it', async () => {
  const changes = []
  const { view } = mountComparison({
    onSelectionChange(request) { changes.push(request) },
    reviews: {
      [REF_ONE]: historicalReview(),
      [REF_TWO]: historicalReview({
        candidate: candidateSummary({ candidateRef: REF_TWO, diffSha256: DIGEST_TWO }),
        evidence: [],
        verdict: null,
      }),
    },
  })
  await flush()
  const from = findByClass(view.root, 'wwc-strongflow-candidate-comparison-from')
  from.value = REF_TWO
  from.emit('change')
  await flush()
  assert.deepEqual(changes, [{
    before: identity(REF_TWO, DIGEST_TWO),
    after: afterOne,
  }])
  const alert = findByClass(view.root, 'wwc-strongflow-candidate-comparison-alert')
  assert.equal(alert.hidden, true)
  const files = findByClass(view.root, 'wwc-strongflow-candidate-comparison-files')
  assert.equal(files.dataset.known, 'false')
  const evidenceLine = findByClass(view.root, 'wwc-strongflow-candidate-comparison-evidence')
  assert.equal(evidenceLine.dataset.added, '1')
  assert.equal(evidenceLine.dataset.removed, '0')
  assert.equal(evidenceLine.textContent.includes('evd_00000000000000000000000001'), true)
})

test('the rendered summary stays stable when the same snapshot arrives twice', async () => {
  const { view } = mountComparison()
  await flush()
  const summary = findByClass(view.root, 'wwc-strongflow-candidate-comparison-summary')
  const files = findByClass(view.root, 'wwc-strongflow-candidate-comparison-files')
  const rows = [...files.children]
  view.update({
    projection: DEFAULT_PROJECTION,
    candidateFiles: candidateFilesState(),
    requested: { status: 'none' },
    reworkBaselineDigest: null,
  })
  await flush()
  assert.equal(findByClass(view.root, 'wwc-strongflow-candidate-comparison-summary'), summary)
  assert.equal(findByClass(view.root, 'wwc-strongflow-candidate-comparison-files'), files)
  assert.deepEqual([...files.children], rows)
})

test('a Delivery without a frozen Candidate hides the comparison workbench', async () => {
  const { view } = mountComparison({
    projection: projection({
      delivery: { currentCandidate: null },
      currentCandidate: null,
    }),
    history: [],
  })
  await flush()
  assert.equal(
    findByClass(view.root, 'wwc-strongflow-candidate-comparison-controls').hidden,
    true,
  )
  assert.equal(
    findByClass(view.root, 'wwc-strongflow-candidate-comparison-empty').hidden,
    false,
  )
})

test('a Candidate from another Delivery is never offered for selection', async () => {
  const { view } = mountComparison({
    history: [
      historyItem({
        candidate: candidateSummary({
          candidateRef: 'refs/winwincode/candidate/foreign',
          deliverySpecId: 'spec-other-delivery',
        }),
      }),
      historyItem({ isCurrentAtReadCursor: true }),
    ],
  })
  await flush()
  const from = findByClass(view.root, 'wwc-strongflow-candidate-comparison-from')
  assert.deepEqual([...from.children].map(option => option.value), ['baseline', REF_ONE])
  const to = findByClass(view.root, 'wwc-strongflow-candidate-comparison-to')
  assert.deepEqual([...to.children].map(option => option.value), [REF_ONE])
})

test('closing the panel removes its root and rejects later updates', async () => {
  const { view } = mountComparison()
  const root = view.root
  view.close()
  assert.equal(root.parentNode, null)
  assert.throws(() => view.update({
    projection: DEFAULT_PROJECTION,
    candidateFiles: candidateFilesState(),
    requested: { status: 'none' },
    reworkBaselineDigest: null,
  }))
  view.close()
})
