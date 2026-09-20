import assert from 'node:assert/strict'
import { createQueryCache } from '@winwincode/browser-core/query-cache'
import { spawnSync } from 'node:child_process'
import { pathToFileURL } from 'node:url'
import { resolve } from 'node:path'
import test from 'node:test'
import { JSDOM } from 'jsdom'

const root = resolve(import.meta.dirname, '..')
const compiled = spawnSync('corepack', [
  'pnpm', 'exec', 'tsc', '-p', 'apps/client/tsconfig.strongflow-review-tests.json',
  '--pretty', 'false', '--incremental', 'false',
], { cwd: root, encoding: 'utf8' })
assert.equal(compiled.status, 0, `review UI did not compile:\n${compiled.stdout}${compiled.stderr}`)

const cache = resolve(root, '.cache/strongflow-review-tests')
const reviewModule = await import(pathToFileURL(resolve(cache, 'strongflow-review-view-model.js')).href)
const detailModule = await import(pathToFileURL(resolve(cache, 'strongflow-review-detail.js')).href)
const annotationsModule = await import(pathToFileURL(resolve(cache, 'strongflow-review-annotations.js')).href)
const redactionModule = await import(pathToFileURL(resolve(cache, 'public-redaction.js')).href)
const {
  classifyPreview,
  createStrongFlowReviewViewModel,
  reviewProgress,
  solutionReviewDecisionResolution,
} = reviewModule
const { deliveryReportText } = detailModule
const { candidateCanApply } = await import(pathToFileURL(resolve(cache, 'candidate-application.js')).href)
const { diffLines, renderDiff } = await import(pathToFileURL(resolve(cache, 'diff-preview.js')).href)
const { createStrongFlowReviewAnnotations } = annotationsModule
const { redactPublicText, publicErrorText } = redactionModule

const deliveryId = 'dlv_00000000000000000000000042'
const workRunId = 'wrn_00000000000000000000000042'
const evidenceId = 'evd_00000000000000000000000042'
const artifactId = 'art_00000000000000000000000042'
const candidateRef = `git-candidate:sha256:${'c'.repeat(64)}`
const scope = {
  kind: 'repository',
  organizationId: 'org_00000000000000000000000001',
  workspaceId: 'wsp_00000000000000000000000001',
  projectId: 'prj_00000000000000000000000001',
  repositoryId: 'rep_00000000000000000000000001',
}
const readCursor = {
  token: `sfread_${'1'.padStart(32, '0')}`,
  scope,
  deliveryId,
  deliveryRevision: 2,
  runtimeLedgerRevision: 1,
  runtimeAcceptedSequence: 1,
  publicationRevision: 0,
  eventCursor: { scope, stream: { kind: 'delivery', deliveryId }, sequence: 0, eventId: null },
}
const candidate = {
  candidateRef,
  deliverySpecId: 'spec-review',
  deliverySpecRevision: 1,
  producerWorkRunId: workRunId,
  producerSessionBindingId: 'binding-1',
  candidateCommitId: 'abc1234567890abcdef1234567890abcdef123456',
  candidateTreeId: 'bbb1234567890abcdef1234567890abcdef123456',
  diffSha256: `sha256:${'b'.repeat(64)}`,
  frozenAt: '2026-09-11T00:00:00.000Z',
}
const evidence = {
  id: evidenceId,
  deliverySpecId: 'spec-review',
  deliverySpecRevision: 1,
  workRunId,
  sessionBindingId: 'binding-1',
  candidateRef,
  type: 'command',
  sourceRef: 'runtime:command:42',
  createdAt: '2026-09-11T00:01:00.000Z',
}
const acceptanceCriteria = [
  { id: 'criterion-one', description: 'Tests pass', required: true, verificationMethod: 'node --test' },
  { id: 'criterion-two', description: 'Review passes', required: true, verificationMethod: null },
  { id: 'criterion-three', description: 'Docs checked', required: false, verificationMethod: null },
]
const verdict = {
  id: 'verdict-1', deliverySpecId: 'spec-review', deliverySpecRevision: 1,
  candidateRef, producedAt: '2026-09-11T00:02:00.000Z', status: 'fail',
  unresolvedFindings: ['review pending'],
  criteria: [
    { resultId: 'result-1', criterionId: 'criterion-one', verdict: 'pass', explanation: 'passed', evidenceRefs: [evidenceId], evaluatedAt: '2026-09-11T00:02:00.000Z' },
    { resultId: 'result-2', criterionId: 'criterion-two', verdict: 'fail', explanation: 'failed', evidenceRefs: [evidenceId], evaluatedAt: '2026-09-11T00:02:00.000Z' },
  ],
}
const solutionReview = {
  deliveryId,
  deliverySpecId: 'spec-review',
  deliverySpecRevision: 1,
  planningWorkRunId: workRunId,
  planningSessionBindingId: 'binding-1',
  reviewWorkRunId: null,
  attentionItemId: 'att_00000000000000000000000042',
  reviewSetSha256: `sha256:${'a'.repeat(64)}`,
  reviewStatus: 'pending',
  decision: null,
  comments: null,
  requestedChanges: null,
  reviewerId: null,
  reviewedAt: null,
  solutionId: 'solution-review',
  summary: 'Keep the current review path.',
  approach: ['Read the sealed delivery projection.'],
  components: [],
  connections: [],
  architectureDiagram: { id: 'architecture', kind: 'system-architecture', title: 'Architecture', nodes: [], edges: [] },
  processDiagram: { id: 'process', kind: 'process-flow', title: 'Process', nodes: [], edges: [] },
  risks: [],
  unresolvedItems: [],
  workItemProposals: [{
    id: 'wit_00000000000000000000000042', title: 'Review', goal: 'Review the solution.',
    criterionIds: ['criterion-one'], dependsOn: [],
  }],
}

test('applying requires current candidate, current specification and resolvable passing evidence', () => {
  const state = { status: 'ready', detail: {
    requirements: { deliverySpecId: 'spec-review', deliverySpecRevision: 1, acceptanceCriteria },
    currentCandidate: candidate, evidence: [evidence],
    verdict: { ...verdict, status: 'pass', unresolvedFindings: [], criteria: verdict.criteria.map(item => ({ ...item, verdict: 'pass' })) },
  } }
  assert.equal(candidateCanApply(state), true)
  for (const changed of [
    { ...state, status: 'refreshing' },
    { ...state, detail: { ...state.detail, evidence: [] } },
    { ...state, detail: { ...state.detail, currentCandidate: { ...candidate, deliverySpecRevision: 2 } } },
    { ...state, detail: { ...state.detail, verdict } },
    { ...state, detail: { ...state.detail, verdict: { ...state.detail.verdict, candidateRef: 'other-version' } } },
    { ...state, detail: { ...state.detail, verdict: { ...state.detail.verdict, criteria: [state.detail.verdict.criteria[0]] } } },
    { ...state, detail: { ...state.detail, evidence: [{ ...evidence, candidateRef: 'other-version' }] } },
  ]) assert.equal(candidateCanApply(changed), false)
})

function response(query, result, page = { hasMore: false, nextCursor: null }) {
  return { schemaVersion: 'winwincode/v1', requestId: 'req_1', query, result, page }
}

function fixtureClient(pendingSolution = null) {
  const calls = []
  let currentSolution = pendingSolution
  return {
    calls,
    async command(request) {
      calls.push(request)
      currentSolution = {
        ...currentSolution,
        reviewStatus: 'approved', decision: 'approve', comments: 'Looks good.',
        reviewerId: 'usr_00000000000000000000000001', reviewedAt: '2026-09-11T00:03:00.000Z',
      }
      return {
        schemaVersion: 'winwincode/v1', requestId: request.requestId,
        command: request.command, outcome: 'completed', previousRevision: 2,
        currentRevision: 3, result: {},
      }
    },
    async query(request) {
      calls.push(request)
      switch (request.query) {
        case 'delivery.get':
          return response(request.query, {
            kind: 'delivery_detail', schemaVersion: 'winwincode/v1', deliveryId,
            deliveryRevision: 2, readCursor, status: 'ready',
            ownership: scope, requirements: { title: '审核真实产物', acceptanceCriteria },
            attention: [
              { id: 'att_1', title: '处理失败命令', status: 'open' },
              ...(currentSolution === null ? [] : [{
                id: currentSolution.attentionItemId,
                title: '审核当前方案',
                status: currentSolution.reviewStatus === 'pending' ? 'open' : 'resolved',
              }]),
            ],
            evidence: [evidence], currentCandidate: candidate, verdict,
            solutionReview: currentSolution, diagramExecution: null, publication: null,
          })
        case 'candidate.list':
          return response(request.query, {
            kind: 'candidate_history_page', readCursor,
            items: [{
              candidate, availability: 'released', isCurrentAtReadCursor: true,
              firstSeenDeliveryRevision: 1, lastSeenDeliveryRevision: 2,
              reviewDeliveryRevision: 2,
            }],
          })
        case 'candidate.files.list':
          return response(request.query, {
            kind: 'candidate_file_page', readCursor, candidate,
            items: request.page.cursor === null ? [
              { path: 'src/main.ts', oldPath: null, status: 'modified', encoding: 'utf-8', binary: false, additions: 1, deletions: 1 },
              { path: 'report.svg', oldPath: null, status: 'added', encoding: 'utf-8', binary: false, additions: 1, deletions: 0 },
            ] : [
              { path: 'asset.bin', oldPath: null, status: 'added', encoding: 'binary', binary: true, additions: null, deletions: null },
            ],
          }, request.page.cursor === null
            ? { hasMore: true, nextCursor: 'files-2' }
            : { hasMore: false, nextCursor: null })
        case 'workrun.get':
          return response(request.query, { runs: [{ id: workRunId, productSessionId: 'psn_00000000000000000000000042' }] })
        case 'runtime.projection.get':
          return response(request.query, {
            sessions: Array.from({ length: 5 }, (_, sessionIndex) => ({
              productSessionId: `psn_${String(sessionIndex + 42).padStart(26, '0')}`,
              sessionBindingId: `binding-${String(sessionIndex + 1)}`, workRunId, attempt: 1,
              asOfSequence: sessionIndex + 1,
              plan: sessionIndex === 0 ? {
                sourceRef: 'runtime:plan:42', itemId: 'plan-42', explanation: null,
                text: null, complete: false,
                items: [
                  { step: 'Inspect', status: 'completed' },
                  { step: 'Implement', status: 'in_progress' },
                  { step: 'Verify', status: 'pending' },
                ],
              } : null,
              activities: Array.from({ length: sessionIndex === 0 ? 101 : 1 }, (_, activityIndex) => ({
                callId: sessionIndex === 0 && activityIndex === 0 ? 'call-42' : `call-${sessionIndex}-${activityIndex}`,
                activityType: 'command', command: 'node --test',
                status: sessionIndex === 0 ? 'failed' : 'completed',
                outcome: sessionIndex === 0 ? 'task-failed' : 'succeeded',
                exitCode: sessionIndex === 0 ? 1 : 0,
                sourceRef: sessionIndex === 0 && activityIndex === 0
                  ? 'runtime:command:42'
                  : `runtime:command:${sessionIndex}:${activityIndex}`,
              })),
            })),
          })
        case 'candidate.diff.get': {
          const offset = request.parameters.offset
          const text = offset === 0 ? 'abc' : 'def'
          return response(request.query, {
            ...request.parameters, kind: 'candidate_diff_chunk', candidate,
            path: request.parameters.path, oldPath: null, status: 'modified', binary: false,
            contentEncoding: 'utf-8', encoding: 'base64',
            dataBase64: Buffer.from(text).toString('base64'),
            mediaType: 'application/vnd.winwincode.git-diff',
            fileDiffSha256: `sha256:${'d'.repeat(64)}`, readCursor,
            offset, returnedBytes: 3, totalBytes: 6, nextOffset: offset === 0 ? 3 : null,
          })
        }
        case 'evidence.get':
          return response(request.query, {
            kind: 'evidence_detail', evidence, outcome: 'failed', readCursor,
            artifactAccess: { state: 'available', items: [{
              artifactId, kind: 'log', digest: `sha256:${'e'.repeat(64)}`,
              fileName: 'failure.log', mediaType: 'text/plain', sizeBytes: 6,
              previewMode: 'inline_text',
              provenance: { candidateRef, deliveryId, deliveryRevision: 2, evidenceId, sessionBindingId: 'binding-1', workRunId },
            }] },
          })
        case 'evidence.artifact.content.get': {
          const offset = request.parameters.offset
          const text = offset === 0 ? 'log' : 'end'
          return response(request.query, {
            kind: 'evidence_artifact_content_chunk', state: 'available', readCursor,
            artifact: {
              artifactId, kind: 'log', digest: `sha256:${'e'.repeat(64)}`,
              fileName: 'failure.log', mediaType: 'text/plain', sizeBytes: 6,
              previewMode: 'inline_text',
              provenance: { candidateRef, deliveryId, deliveryRevision: 2, evidenceId, sessionBindingId: 'binding-1', workRunId },
            },
            evidence, contentEncoding: 'utf-8', previewMode: 'inline_text',
            dataBase64: Buffer.from(text).toString('base64'), encoding: 'base64',
            offset, returnedBytes: 3, totalBytes: 6, nextOffset: offset === 0 ? 3 : null,
            truncated: offset === 0,
          })
        }
        case 'candidate.review.get':
          return response(request.query, {
            kind: 'candidate_historical_review', readCursor, candidate,
            availability: 'released', displayOnly: true, currentAuthorization: false,
            firstSeenDeliveryRevision: 1, lastSeenDeliveryRevision: 2,
            reviewDeliveryRevision: 2, evidence: [evidence], verdict: null,
          })
        default:
          throw new Error(`unexpected query ${request.query}`)
      }
    },
  }
}

test('review preview classes keep executable and binary content out of the DOM path', () => {
  assert.equal(classifyPreview({ path: 'report.svg', binary: false, encoding: 'utf-8' }).previewClass, 'executable-document')
  assert.equal(classifyPreview({ path: 'asset.bin', binary: true, encoding: 'binary' }).previewClass, 'binary')
  assert.equal(classifyPreview({ path: 'src/main.ts', binary: false, encoding: 'utf-8' }).previewClass, 'text')
})

test('browser-facing runtime text redacts credentials, paths, and internal identities', () => {
  const candidate = 'git-candidate:sha256:' + 'a'.repeat(64)
  const value = redactPublicText([
    'aUtHoRiZaTiOn: BeArEr super-secret-token',
    'TOKEN=top-secret',
    'path=/Users/alice/project/.env',
    'path=C:\\Users\\alice\\project\\.env',
    `candidate=${candidate} and again ${candidate}`,
    'sourceRef=runtime:command:42\nsource_ref: runtime:command:43',
    'workRun=wrn_00000000000000000000000001\nworkRun=wrn_00000000000000000000000002',
    '用户验收条件：登录后显示相对路径 src/app.ts，短 commit 1a2b3c4d',
    'diff:\n- const value = 1\n+ const value = 2',
  ].join('\n'))
  assert.doesNotMatch(value, /super-secret-token|top-secret|\/Users\/alice|C:\\Users\\alice|git-candidate:|runtime:command:|sourceRef=|source_ref:|wrn_000/u)
  assert.match(value, /\[REDACTED\]|\[REDACTED PATH\]|\[CANDIDATE\]|\[INTERNAL ID\]/u)
  assert.match(value, /用户验收条件：登录后显示相对路径 src\/app\.ts，短 commit 1a2b3c4d/u)
  assert.match(value, /diff:\n- const value = 1\n\+ const value = 2/u)
  assert.match(publicErrorText({ code: 'INTERNAL_ERROR', message: 'raw server detail' }, '读取产物'), /读取产物，请重试/u)
  assert.doesNotMatch(publicErrorText({ code: 'INTERNAL_ERROR', message: 'raw server detail' }, '读取产物'), /raw server detail/u)
})

test('working-plan and accepted-criterion progress are separate exact counts', () => {
  const progress = reviewProgress({
    requirements: { acceptanceCriteria },
    verdict,
  }, [{ sessions: [{
    attempt: 1,
    asOfSequence: 7,
    plan: {
      sourceRef: 'runtime:plan:42',
      items: [
        { step: 'Inspect', status: 'completed' },
        { step: 'Implement', status: 'in_progress' },
        { step: 'Verify', status: 'pending' },
      ],
    },
  }] }])
  assert.deepEqual(progress, {
    workingPlan: { completed: 1, inProgress: 1, pending: 1, total: 3, sourceRef: 'runtime:plan:42' },
    acceptedCriteria: { accepted: 1, failed: 1, inconclusive: 0, infraError: 0, pending: 1, total: 3 },
  })
  assert.doesNotMatch(JSON.stringify(progress), /percent|percentage|%/u)
})

test('solution review decisions use the exact canonical three-action payload', () => {
  const approve = solutionReviewDecisionResolution(solutionReview, 'approve', ' Looks good. ')
  assert.equal(approve, JSON.stringify({
    schemaVersion: 1,
    protocol: 'winwincode.solution-review-decision.v1',
    deliveryId,
    deliverySpecId: 'spec-review',
    deliverySpecRevision: 1,
    attentionItemId: solutionReview.attentionItemId,
    reviewSetSha256: solutionReview.reviewSetSha256,
    action: 'approve',
    comments: 'Looks good.',
    requestedChanges: null,
  }))
  assert.deepEqual(
    JSON.parse(solutionReviewDecisionResolution(solutionReview, 'request_changes', 'First\n\n Second ')).requestedChanges,
    ['First', 'Second'],
  )
  assert.equal(
    JSON.parse(solutionReviewDecisionResolution(solutionReview, 'reject', 'Wrong scope.')).action,
    'reject',
  )
  assert.throws(
    () => solutionReviewDecisionResolution(solutionReview, 'approve', '  '),
    error => error.code === 'SOLUTION_REVIEW_NOTE_REQUIRED',
  )
})

test('solution review decision binds the current Attention and refreshes after completion', async () => {
  const client = fixtureClient(solutionReview)
  let request = 0
  const model = createStrongFlowReviewViewModel({
    client,
    actor: { kind: 'user', id: 'usr_00000000000000000000000001' },
    scope,
    deliveryId,
    nextRequestId: () => `req_${String(++request).padStart(26, '0')}`,
  })
  await model.start()
  assert.equal(await model.decideSolutionReview('approve', 'Looks good.'), 'completed')
  const sent = client.calls.find(call => call.command === 'delivery.resolve_attention')
  assert.equal(sent.expectedRevision, 2)
  assert.equal(sent.payload.decision, 'resolve')
  assert.deepEqual(JSON.parse(sent.payload.resolution), {
    schemaVersion: 1,
    protocol: 'winwincode.solution-review-decision.v1',
    deliveryId,
    deliverySpecId: 'spec-review',
    deliverySpecRevision: 1,
    attentionItemId: solutionReview.attentionItemId,
    reviewSetSha256: solutionReview.reviewSetSha256,
    action: 'approve',
    comments: 'Looks good.',
    requestedChanges: null,
  })
  assert.equal(model.state.detail.solutionReview.reviewStatus, 'approved')
  await assert.rejects(
    model.decideSolutionReview('reject', 'No longer current.'),
    error => error.code === 'SOLUTION_REVIEW_STALE',
  )
  model.close()
})

test('delivery report keeps AC results while excluding free-form content and paths', () => {
  const report = deliveryReportText({
    detail: {
      deliveryId,
      deliveryRevision: 2,
      currentCandidate: candidate,
      requirements: {
        title: 'token sk-test-secret',
        acceptanceCriteria: acceptanceCriteria.map((criterion, index) => ({
          ...criterion,
          description: index === 0 ? '/Users/alice/private/review.log' : criterion.description,
        })),
      },
      evidence: [{ ...evidence, sourceRef: '/Users/alice/private/raw.log' }],
      verdict: {
        ...verdict,
        unresolvedFindings: ['credential ghp_private_token'],
        criteria: verdict.criteria.map(result => ({
          ...result,
          explanation: 'raw stdout /Users/alice/private',
        })),
      },
    },
  })
  assert.match(report, /1\. required; result=pass; evidence=1; verification=mapped/u)
  assert.match(report, /3\. optional; result=pending; evidence=0; verification=unmapped/u)
  assert.match(report, /Residual risk: required_not_pass=1; unresolved_findings=1/u)
  assert.doesNotMatch(report, /sk-test-secret|ghp_private_token|\/Users\/alice|raw stdout/u)
})

test('review reads bounded current facts, continues ranges, and preserves historical authority', async () => {
  const client = fixtureClient()
  let request = 0
  const model = createStrongFlowReviewViewModel({
    client,
    actor: { kind: 'human', id: 'hum_00000000000000000000000001' },
    scope,
    deliveryId,
    nextRequestId: () => `req_${String(++request).padStart(26, '0')}`,
  })

  await model.start()
  assert.equal(model.state.status, 'ready')
  assert.equal(model.state.files.length, 2)
  assert.equal(model.state.filesTruncated, true)
  await model.continueFiles()
  assert.equal(model.state.files.length, 3)
  assert.equal(model.state.filesTruncated, false)
  assert.equal(model.state.segments.length, 4)
  const workRunReads = client.calls.filter(call => call.query === 'workrun.get')
  assert.equal(workRunReads.length, 1)
  assert.equal(workRunReads[0].parameters.workRunId, null, 'include active review runs before they produce evidence')
  assert.equal(model.state.segments[0].activities.length, 100)
  assert.equal(model.state.segments[0].truncated, true)
  assert.deepEqual(model.state.progress, {
    workingPlan: { completed: 1, inProgress: 1, pending: 1, total: 3, sourceRef: 'runtime:plan:42' },
    acceptedCriteria: { accepted: 1, failed: 1, inconclusive: 0, infraError: 0, pending: 1, total: 3 },
  })
  assert.equal(model.state.segments[0].activities[0].sourceRef, 'runtime:command:42')
  assert.equal(model.snippetFor(model.state.segments[0].key, 'call-42').workRunId, workRunId)

  await model.openPreview('src/main.ts')
  assert.equal(model.previewText('src/main.ts'), 'abc', 'the first chunk is not duplicated')
  await Promise.all([model.openPreview('src/main.ts'), model.openPreview('src/main.ts')])
  assert.equal(model.previewText('src/main.ts'), 'abc', 'overlapping first-chunk reads are deduplicated')
  await model.continuePreview('src/main.ts')
  assert.equal(model.previewText('src/main.ts'), 'abcdef')
  await model.openPreview('report.svg')
  assert.equal(model.previewText('report.svg'), null)

  await model.openEvidenceDetail(evidenceId)
  assert.equal(model.state.evidence[0].artifactState, 'available')
  await model.openArtifact(evidenceId, artifactId)
  await model.continueArtifact(evidenceId, artifactId)
  const download = model.artifactDownload(evidenceId, artifactId)
  assert.equal(Buffer.from(download.bytes).toString('utf8'), 'logend')

  await model.openHistoricalReview(candidateRef)
  assert.equal(model.state.history[0].review.currentAuthorization, false)
  assert.equal(model.state.history[0].review.availability, 'released')
  assert.equal(client.calls.some(call => call.query === 'evidence.artifact.content.get' && call.parameters.length === 64 * 1024), true)
  assert.equal(client.calls.some(call => 'command' in call), false)
  model.close()
})

test('review refresh invalidates cached delivery progress', async () => {
  const fixture = fixtureClient()
  let title = '执行中'
  const cache = createQueryCache({ client: { close() {}, async query(request) {
    const result = await fixture.query(request)
    return { ...result, requestId: request.requestId, result: request.query === 'delivery.get'
      ? { ...result.result, requirements: { ...result.result.requirements, title } } : result.result }
  } } })
  let request = 0
  const model = createStrongFlowReviewViewModel({ client: cache.client,
    actor: { kind: 'human', id: 'hum_00000000000000000000000001' }, scope, deliveryId,
    nextRequestId: () => `req_${String(++request).padStart(26, '0')}` })
  await model.start()
  assert.equal(model.state.detail.requirements.title, '执行中')
  title = '验收通过'
  await model.refresh()
  assert.equal(model.state.detail.requirements.title, '验收通过')
  model.close()
  cache.close()
})

test('stale preview responses do not enter a refreshed candidate', async () => {
  const fixture = fixtureClient()
  let deliveryReads = 0
  let release
  const gate = new Promise(resolve => { release = resolve })
  const client = { async query(request) {
    if (request.query === 'candidate.diff.get' && request.parameters.offset === 0) await gate
    const response = await fixture.query(request)
    if (request.query === 'delivery.get' && ++deliveryReads > 1) {
      response.result = { ...response.result, readCursor: {
        ...response.result.readCursor,
        token: `sfread_${'2'.padStart(32, '0')}`,
      } }
    }
    return response
  } }
  let request = 0
  const model = createStrongFlowReviewViewModel({ client,
    actor: { kind: 'human', id: 'hum_00000000000000000000000001' }, scope, deliveryId,
    nextRequestId: () => `req_${String(++request).padStart(26, '0')}` })
  await model.start()
  const pending = model.openPreview('src/main.ts')
  await Promise.resolve()
  await model.refresh()
  release()
  await pending
  assert.equal(model.previewText('src/main.ts'), null)
  model.close()
})

test('application confirmation accepts only the current passing candidate', async () => {
  const fixture = fixtureClient()
  const commands = []
  let accepted = false
  const client = {
    async query(request) {
      const response = await fixture.query(request)
      if (request.query === 'delivery.get') response.result = { ...response.result,
        verdict: { ...verdict, status: 'pass' },
        attention: accepted ? [] : [{ id: 'att_1', type: 'delivery_approval', status: 'open' }] }
      return response
    },
    async command(request) {
      commands.push(request); accepted = true
      return { requestId: request.requestId, command: request.command, outcome: 'completed' }
    },
  }
  let request = 0
  const model = createStrongFlowReviewViewModel({ client,
    actor: { kind: 'human', id: 'hum_00000000000000000000000001' }, scope, deliveryId,
    nextRequestId: () => `req_${String(++request).padStart(26, '0')}` })
  await model.start()
  await assert.rejects(model.acceptDelivery('foreign-commit'), /版本或验收结果已变化/u)
  assert.equal(commands.length, 0)
  await model.acceptDelivery(candidate.candidateCommitId)
  assert.equal(commands[0].command, 'delivery.resolve_attention')
  assert.equal(commands[0].expectedRevision, 2)
  assert.equal(commands[0].payload.attentionItemId, 'att_1')
  await model.acceptDelivery(candidate.candidateCommitId)
  assert.equal(commands.length, 1, 'completed approval is not submitted again')
  model.close()
})

test('review pins are bounded and error citations retain their source identity', () => {
  const annotations = createStrongFlowReviewAnnotations()
  annotations.pin({ key: `artifact:${artifactId}`, kind: 'artifact', label: 'failure.log', retention: 'available' })
  annotations.setPinRetention(`artifact:${artifactId}`, 'released')
  assert.equal(annotations.state.pins[0].availabilityChanged, true)
  const result = annotations.attachSnippet(
    { id: 'att_1', title: '处理失败命令' },
    { sourceRef: 'runtime:command:42', workRunId, sessionBindingId: 'binding-1', productSessionId: 'psn_1', command: 'node --test', exitCode: 1, outcome: 'task-failed' },
  )
  assert.equal(result.attached, true)
  assert.equal(annotations.state.snippets[0].snippet.sourceRef, 'runtime:command:42')
  annotations.close()
})


test('unified diff numbering distinguishes headers, hunks, additions and removals', () => {
  const source = [
    'diff --git a/file b/file', '--- a/file', '+++ b/file',
    '@@ -8,2 +8,3 @@', ' unchanged', '-removed', '+added', '+++literal source',
    '\\ No newline at end of file', '@@ -20,0 +22,1 @@', '+new line',
    'diff --git a/other b/other', '--- a/other', '+++ b/other',
    '@@ -1 +0,0 @@', '-deleted', '',
  ].join('\n')
  const lines = diffLines(source)
  assert.deepEqual(lines.filter(line => ['context', 'added', 'removed'].includes(line.kind))
    .map(({ kind, before, after }) => [kind, before, after]), [
      ['context', 8, 8], ['removed', 9, null], ['added', null, 9], ['added', null, 10],
      ['added', null, 22], ['removed', 1, null],
    ])
  assert.equal(lines.find(line => line.text === '+++ b/file').kind, 'meta')
  assert.equal(lines.find(line => line.text === '+++literal source').kind, 'added')
  assert.equal(lines.find(line => line.text.startsWith('\\ No newline')).kind, 'meta')
})

test('rendered diff marks line changes while keeping code text inert', () => {
  const dom = new JSDOM('<!doctype html><body></body>')
  const code = renderDiff(dom.window.document, [
    '@@ -1,2 +1,2 @@',
    ' context <safe>',
    '-const value = "old"',
    '+const value = "new"',
  ].join(String.fromCharCode(10)))
  const rows = [...code.querySelectorAll('.wwc-diff-line')]
  assert.equal(rows.filter(row => row.dataset.diffKind === 'context').length, 1)
  assert.equal(rows.find(row => row.dataset.diffKind === 'removed').querySelector('.wwc-diff-marker').textContent, '−')
  assert.equal(rows.find(row => row.dataset.diffKind === 'added').querySelector('.wwc-diff-marker').textContent, '+')
  assert.equal(code.querySelectorAll('.wwc-diff-inline-change').length, 2)
  assert.equal(code.querySelector('.wwc-diff-inline-change').textContent, 'old')
  assert.match(code.textContent, /context <safe>/u)
  assert.equal(code.querySelector('script'), null)
  const standalone = renderDiff(dom.window.document, '@@ -1 +1 @@\n-remove\n keep\n+add\n')
  assert.deepEqual([...standalone.querySelectorAll('.wwc-diff-content')].map(row => row.textContent), [
    ' @@ -1 +1 @@\n', '−remove\n', ' keep\n', '+add\n',
  ])
})
