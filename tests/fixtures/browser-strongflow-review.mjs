import { createStrongFlowReviewAnnotations } from '/module/strongflow-review-annotations.js'
import { mountStrongFlowReviewDetail } from '/module/strongflow-review-detail.js'
import { createStrongFlowReviewViewModel } from '/module/strongflow-review-view-model.js'

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
  token: `sfread_${'1'.padStart(32, '0')}`, scope, deliveryId,
  deliveryRevision: 2, runtimeLedgerRevision: 1, runtimeAcceptedSequence: 1,
  publicationRevision: 0,
  eventCursor: { scope, stream: { kind: 'delivery', deliveryId }, sequence: 0, eventId: null },
}
const candidate = {
  candidateRef, deliverySpecId: 'spec-review', deliverySpecRevision: 1,
  producerWorkRunId: workRunId, producerSessionBindingId: 'binding-1',
  candidateCommitId: 'abc1234567890abcdef1234567890abcdef123456',
  candidateTreeId: 'bbb1234567890abcdef1234567890abcdef123456',
  diffSha256: `sha256:${'b'.repeat(64)}`, frozenAt: '2026-09-11T00:00:00.000Z',
}
const evidence = {
  id: evidenceId, deliverySpecId: 'spec-review', deliverySpecRevision: 1,
  workRunId, sessionBindingId: 'binding-1', candidateRef,
  type: 'command', sourceRef: 'runtime:command:42', createdAt: '2026-09-11T00:01:00.000Z',
}
const acceptanceCriteria = [
  { id: 'criterion-one', description: 'Tests pass', required: true, verificationMethod: 'node --test' },
  { id: 'criterion-two', description: 'Review passes', required: true, verificationMethod: null },
]
const verdict = {
  id: 'verdict-1', deliverySpecId: 'spec-review', deliverySpecRevision: 1,
  candidateRef, producedAt: '2026-09-11T00:02:00.000Z', status: 'pass', unresolvedFindings: [],
  criteria: [{
    resultId: 'result-1', criterionId: 'criterion-one', verdict: 'pass', explanation: 'passed',
    evidenceRefs: [evidenceId], evaluatedAt: '2026-09-11T00:02:00.000Z',
  }],
}
const pendingSolutionReview = {
  deliveryId, deliverySpecId: 'spec-review', deliverySpecRevision: 1,
  planningWorkRunId: workRunId, planningSessionBindingId: 'binding-1', reviewWorkRunId: null,
  attentionItemId: 'att_00000000000000000000000042',
  reviewSetSha256: `sha256:${'a'.repeat(64)}`,
  reviewStatus: 'pending', decision: null, comments: null, requestedChanges: null,
  reviewerId: null, reviewedAt: null, solutionId: 'solution-review',
  summary: 'Use the existing StrongFlow review path.',
  approach: ['Read the sealed plan.', 'Keep Controller authority.'],
  components: [], connections: [],
  architectureDiagram: { id: 'architecture', kind: 'system-architecture', title: 'Architecture', nodes: [], edges: [] },
  processDiagram: { id: 'process', kind: 'process-flow', title: 'Process', nodes: [], edges: [] },
  risks: ['Confirm the final scope.'], unresolvedItems: [],
  workItemProposals: [{
    id: 'wit_00000000000000000000000042', title: 'Review', goal: 'Review the solution.',
    criterionIds: ['criterion-one'], dependsOn: [],
  }],
}
const page = { hasMore: false, nextCursor: null }
const response = (request, result) => ({
  schemaVersion: 'winwincode/v1', requestId: request.requestId,
  query: request.query, result, page,
})
const descriptor = {
  artifactId, kind: 'report', digest: `sha256:${'e'.repeat(64)}`,
  fileName: 'report.html', mediaType: 'text/html', sizeBytes: 48,
  previewMode: 'inline_text',
  provenance: { candidateRef, deliveryId, deliveryRevision: 2, evidenceId, sessionBindingId: 'binding-1', workRunId },
}

const queries = []
const commands = []
let currentSolutionReview = pendingSolutionReview
const controlPlane = {
  async command(request) {
    commands.push(structuredClone(request))
    currentSolutionReview = {
      ...currentSolutionReview,
      reviewStatus: 'approved', decision: 'approve', comments: '同意当前方案。',
      reviewerId: 'usr_00000000000000000000000001', reviewedAt: '2026-09-11T00:03:00.000Z',
    }
    return {
      schemaVersion: 'winwincode/v1', requestId: request.requestId, command: request.command,
      outcome: 'completed', previousRevision: 2, currentRevision: 3, result: {},
    }
  },
  async query(request) {
    queries.push(structuredClone(request))
    switch (request.query) {
      case 'delivery.get':
        return response(request, {
          kind: 'delivery_detail', schemaVersion: 'winwincode/v1', deliveryId,
          deliveryRevision: 2, readCursor, status: 'ready',
          ownership: scope, requirements: { title: '审核真实产物', acceptanceCriteria, maxReworkAttempts: 2 },
          attention: [
            { id: 'att_1', title: '处理失败命令', status: 'open' },
            { id: currentSolutionReview.attentionItemId, title: '审核当前方案',
              status: currentSolutionReview.reviewStatus === 'pending' ? 'open' : 'resolved' },
          ],
          evidence: [evidence], currentCandidate: candidate, verdict,
          solutionReview: currentSolutionReview, diagramExecution: null, publication: null,
        })
      case 'candidate.list':
        return response(request, {
          kind: 'candidate_history_page', readCursor,
          items: [{ candidate, availability: 'released', isCurrentAtReadCursor: true,
            firstSeenDeliveryRevision: 1, lastSeenDeliveryRevision: 2, reviewDeliveryRevision: 2 }],
        })
      case 'candidate.files.list':
        return response(request, {
          kind: 'candidate_file_page', readCursor, candidate,
          items: [{ path: 'report.svg', oldPath: null, status: 'added', encoding: 'utf-8', binary: false, additions: 1, deletions: 0 }],
        })
      case 'workrun.get':
        return response(request, { runs: [{ id: workRunId, productSessionId: 'psn_00000000000000000000000042' }] })
      case 'runtime.projection.get':
        return response(request, { sessions: [{
          productSessionId: 'psn_00000000000000000000000042', sessionBindingId: 'binding-1',
          workRunId, attempt: 1, asOfSequence: 4,
          plan: { sourceRef: 'runtime:plan:42', itemId: 'plan-42', explanation: null, text: null,
            complete: false, items: [
              { step: 'Inspect', status: 'completed' },
              { step: 'Verify', status: 'in_progress' },
            ] },
          activities: [{
            callId: 'call-42', activityType: 'command', command: 'node --test', status: 'failed',
            outcome: 'task-failed', exitCode: 1, sourceRef: 'runtime:command:42',
          }],
        }] })
      case 'candidate.diff.get':
        return response(request, {
          kind: 'candidate_diff_chunk', candidate, path: 'report.svg', oldPath: null,
          status: 'added', binary: false, contentEncoding: 'utf-8', encoding: 'base64',
          dataBase64: btoa('<svg onload="globalThis.pwned=true"></svg>'),
          mediaType: 'application/vnd.winwincode.git-diff', fileDiffSha256: `sha256:${'d'.repeat(64)}`,
          readCursor, offset: 0, returnedBytes: 45, totalBytes: 45, nextOffset: null,
        })
      case 'evidence.get':
        return response(request, {
          kind: 'evidence_detail', evidence, outcome: 'failed', readCursor,
          artifactAccess: { state: 'available', items: [descriptor] },
        })
      case 'evidence.artifact.content.get':
        return response(request, {
          kind: 'evidence_artifact_content_chunk', state: 'available', readCursor,
          artifact: descriptor, evidence, contentEncoding: 'utf-8', previewMode: 'inline_text',
          dataBase64: btoa('<script>globalThis.pwned=true</script>review html'), encoding: 'base64',
          offset: 0, returnedBytes: 48, totalBytes: 48, nextOffset: null, truncated: false,
        })
      case 'candidate.review.get':
        return response(request, {
          kind: 'candidate_historical_review', readCursor, candidate, availability: 'released',
          displayOnly: true, currentAuthorization: false, firstSeenDeliveryRevision: 1,
          lastSeenDeliveryRevision: 2, reviewDeliveryRevision: 2, evidence: [evidence], verdict: null,
        })
      default:
        throw new Error(`unexpected query ${request.query}`)
    }
  },
}

let request = 0
const annotations = createStrongFlowReviewAnnotations()
const model = createStrongFlowReviewViewModel({
  client: controlPlane,
  actor: { kind: 'user', id: 'usr_00000000000000000000000001' },
  scope,
  deliveryId,
  nextRequestId: () => `req_${String(++request).padStart(26, '0')}`,
})
const downloads = []
mountStrongFlowReviewDetail({
  root: document.querySelector('[data-winwincode-client-root]'), model, annotations,
  onDownload(fileName, bytes, mediaType) {
    downloads.push({ fileName, text: new TextDecoder().decode(bytes), mediaType })
  },
})
await model.start()

const waitFor = async predicate => {
  const deadline = Date.now() + 10_000
  while (!predicate()) {
    if (Date.now() >= deadline) throw new Error('timed out waiting for review state')
    await new Promise(resolvePromise => setTimeout(resolvePromise, 20))
  }
}
globalThis.reviewReady = () => model.state.status === 'ready'
globalThis.exerciseReview = async () => {
  document.querySelector('.wwc-review-file-open').click()
  await waitFor(() => model.state.files[0].preview !== null)
  document.querySelector('.wwc-review-activity-cite').click()
  document.querySelector('.wwc-review-evidence-detail').click()
  await waitFor(() => model.state.evidence[0].artifacts.length === 1)
  document.querySelector('.wwc-review-artifact-open').click()
  await waitFor(() => model.state.evidence[0].artifacts[0].nextOffset === null)
  document.querySelector('.wwc-review-artifact-download').click()
  document.querySelector('.wwc-review-history-open').click()
  await waitFor(() => model.state.history[0].review !== null)
  document.querySelector('.wwc-review-report-download').click()
  const beforeDecision = {
    fileClass: document.querySelector('.wwc-review-preview').dataset.reviewClass,
    fileDegraded: document.querySelector('.wwc-review-preview-degraded')?.textContent ?? '',
    artifactClass: document.querySelector('.wwc-review-artifact').dataset.reviewArtifactClass,
    artifactDegraded: document.querySelector('.wwc-review-artifact-degraded')?.textContent ?? '',
    citation: document.querySelector('.wwc-review-annotations-snippet-source')?.textContent ?? '',
    history: document.querySelector('.wwc-review-history-review')?.textContent ?? '',
    currentAuthorization: document.querySelector('.wwc-review-history-review')?.dataset.reviewCurrentAuthorization,
    progress: document.querySelector('.wwc-review-progress')?.textContent ?? '',
    criteria: [...document.querySelectorAll('.wwc-review-criterion')].map(node => ({
      id: node.dataset.reviewCriterionId,
      result: node.dataset.reviewCriterionResult,
      text: node.textContent,
    })),
    report: document.querySelector('.wwc-review-report')?.textContent ?? '',
    solution: document.querySelector('.wwc-review-solution')?.textContent ?? '',
  }
  document.querySelector('.wwc-review-solution-note').value = '同意当前方案。'
  document.querySelector('.wwc-review-solution-approve').click()
  await waitFor(() => model.state.detail.solutionReview.reviewStatus === 'approved')
  return {
    ...beforeDecision,
    scriptCount: document.querySelectorAll('.wwc-review script').length,
    pwned: globalThis.pwned === true,
    solutionStatus: document.querySelector('.wwc-review-solution')?.dataset.reviewSolutionStatus,
    solutionSettled: document.querySelector('.wwc-review-solution')?.textContent ?? '',
    solutionActionsDisabled: [...document.querySelectorAll('.wwc-review-solution-action')]
      .every(control => control.disabled),
    decision: commands.map(command => ({
      command: command.command,
      expectedRevision: command.expectedRevision,
      payload: { ...command.payload, resolution: JSON.parse(command.payload.resolution) },
    })),
    downloads,
    queryNames: queries.map(query => query.query),
  }
}
