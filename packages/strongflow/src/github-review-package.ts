import { createHash, randomUUID } from 'node:crypto'
import {
  lstat,
  mkdir,
  readFile,
  readdir,
  rename,
  rm,
  writeFile,
} from 'node:fs/promises'
import { dirname, join, relative, resolve } from 'node:path'

import {
  STRONGFLOW_GITHUB_DRY_RUN_PROTOCOL,
  STRONGFLOW_GITHUB_PR_PREVIEW_PROTOCOL,
  STRONGFLOW_GITHUB_REVIEW_PACKAGE_PROTOCOL,
  STRONGFLOW_GITHUB_REVIEW_PACKAGE_MEDIA_TYPES,
  STRONGFLOW_GITHUB_REVIEW_PACKAGE_SCHEMA_VERSION,
  parseCriterionResult,
  parseDelivery,
  parseDeliverySpec,
  parseDeliveryVerdict,
  parseEvidenceRef,
  parseFrozenDeliveryCandidate,
  parseStrongFlowGitHubDryRunRecord,
  parseStrongFlowGitHubPublicationContext,
  parseStrongFlowGitHubPullRequestPreview,
  parseStrongFlowGitHubReviewPackageManifest,
  parseStrongFlowPlanReviewContext,
  parseStrongFlowPlanReviewDecision,
  parseStrongFlowPlanReviewDiagram,
  parseStrongFlowPlanReviewSolution,
  type Delivery,
  type EvidenceRef,
  type FrozenDeliveryCandidate,
  type RuntimeEvent,
  type StrongFlowGitHubPublicationContext,
  type StrongFlowGitHubPullRequestPreview,
  type StrongFlowGitHubReviewPackageFile,
  type StrongFlowGitHubReviewPackageManifest,
  type StrongFlowGitHubReviewPackageMediaType,
  type StrongFlowPlanReviewContext,
} from '@winwincode/contracts'

import { freezeAcceptanceVerificationInput } from './acceptance-verification.js'
import {
  resolveDeliveryEvidence,
  type DeliveryEvidenceSource as ResolvableDeliveryEvidenceSource,
} from './candidate-evidence.js'
import {
  StrongFlowDiagramExecutionProjectionError,
  projectStrongFlowDiagramExecution,
} from './diagram-execution-projection.js'
import {
  assertStrongFlowGitHubPublicationReviewCurrent,
} from './github-publication.js'
import { assertStrongFlowPlanReviewCurrent } from './plan-review.js'

export const STRONGFLOW_GITHUB_REVIEW_PACKAGE_PATHS = Object.freeze([
  'requirements/delivery-spec.json',
  'solution/solution.json',
  'solution/plan-review.json',
  'solution/plan-review-decision.json',
  'diagrams/system-architecture.json',
  'diagrams/process-flow.json',
  'candidate/candidate.json',
  'candidate/candidate.diff',
  'verification/evidence-refs.json',
  'verification/criterion-results.json',
  'verification/delivery-verdict.json',
  'github/publication-review.json',
  'github/pull-request-preview.json',
  'github/pull-request-preview.md',
  'github/dry-run.json',
] as const)

export type StrongFlowGitHubReviewPackagePath =
  typeof STRONGFLOW_GITHUB_REVIEW_PACKAGE_PATHS[number]

export type StrongFlowGitHubReviewPackageErrorCode =
  | 'INVALID_INPUT'
  | 'PLAN_REVIEW_STALE'
  | 'PUBLICATION_REVIEW_STALE'
  | 'CANDIDATE_DIFF_INVALID'
  | 'EVIDENCE_UNRESOLVED'
  | 'PACKAGE_INVALID'
  | 'OUTPUT_CONFLICT'
  | 'OUTPUT_IO_ERROR'

export class StrongFlowGitHubReviewPackageError extends Error {
  readonly code: StrongFlowGitHubReviewPackageErrorCode

  constructor(
    code: StrongFlowGitHubReviewPackageErrorCode,
    message: string,
    options?: ErrorOptions,
  ) {
    super(message, options)
    this.name = 'StrongFlowGitHubReviewPackageError'
    this.code = code
  }
}

export interface StrongFlowGitHubReviewPackageContent {
  readonly path: StrongFlowGitHubReviewPackagePath
  readonly mediaType: StrongFlowGitHubReviewPackageMediaType
  readonly content: string
}

export interface GeneratedStrongFlowGitHubReviewPackage {
  readonly schemaVersion: typeof STRONGFLOW_GITHUB_REVIEW_PACKAGE_SCHEMA_VERSION
  readonly manifest: StrongFlowGitHubReviewPackageManifest
  readonly preview: StrongFlowGitHubPullRequestPreview
  readonly files: readonly StrongFlowGitHubReviewPackageContent[]
}

export interface GenerateStrongFlowGitHubReviewPackageInput {
  readonly delivery: Delivery
  readonly candidate: FrozenDeliveryCandidate
  readonly publicationAttentionItemId?: string
  readonly runtimeEvents: readonly RuntimeEvent[]
}

export interface WriteStrongFlowGitHubReviewPackageInput {
  readonly outputDirectory: string
  readonly reviewPackage: GeneratedStrongFlowGitHubReviewPackage
}

export interface WrittenStrongFlowGitHubReviewPackage {
  readonly directory: string
  readonly manifest: StrongFlowGitHubReviewPackageManifest
  readonly reused: boolean
}

const MAX_DIFF_BYTES = 64 * 1_024 * 1_024
const MAX_PACKAGE_BYTES = 128 * 1_024 * 1_024

function packageError(
  code: StrongFlowGitHubReviewPackageErrorCode,
  message: string,
  cause?: unknown,
): never {
  throw new StrongFlowGitHubReviewPackageError(
    code,
    message,
    cause === undefined ? undefined : { cause },
  )
}

function digest(value: string): string {
  return createHash('sha256').update(value).digest('hex')
}

function structuredDigest(value: unknown): string {
  return digest(JSON.stringify(value))
}

function canonicalJson(value: unknown): string {
  return `${JSON.stringify(value, null, 2)}\n`
}

function immutable<Value>(value: Value): Value {
  const clone = structuredClone(value)
  const pending: object[] = []
  if (typeof clone === 'object' && clone !== null) pending.push(clone)
  while (pending.length > 0) {
    const current = pending.pop()!
    if (Object.isFrozen(current)) continue
    Object.freeze(current)
    for (const child of Object.values(current)) {
      if (typeof child === 'object' && child !== null) pending.push(child)
    }
  }
  return clone
}

function equal(left: unknown, right: unknown): boolean {
  return JSON.stringify(left) === JSON.stringify(right)
}

function isRecord(value: unknown): value is Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return false
  const prototype = Object.getPrototypeOf(value)
  return prototype === Object.prototype || prototype === null
}

function exactKeys(value: Readonly<Record<string, unknown>>, keys: readonly string[]): boolean {
  const expected = new Set(keys)
  return Object.keys(value).length === expected.size
    && keys.every(key => Object.hasOwn(value, key))
    && Object.keys(value).every(key => expected.has(key))
}

function file(
  path: StrongFlowGitHubReviewPackagePath,
  mediaType: StrongFlowGitHubReviewPackageMediaType,
  content: string,
): StrongFlowGitHubReviewPackageContent {
  return Object.freeze({ path, mediaType, content })
}

function fileMetadata(
  value: StrongFlowGitHubReviewPackageContent,
): StrongFlowGitHubReviewPackageFile {
  return Object.freeze({
    path: value.path,
    mediaType: value.mediaType,
    sha256: digest(value.content),
    bytes: Buffer.byteLength(value.content),
  })
}

function authoritativeDiff(
  candidate: FrozenDeliveryCandidate,
  runtimeEvents: readonly RuntimeEvent[],
): string {
  const matches = runtimeEvents.filter(event => (
    event.kind === 'diff.updated'
    && typeof event.data.unified_diff === 'string'
    && digest(event.data.unified_diff) === candidate.diffSha256
  ))
  const content = matches.at(-1)?.data.unified_diff
  if (typeof content !== 'string' || Buffer.byteLength(content) > MAX_DIFF_BYTES) {
    return packageError(
      'CANDIDATE_DIFF_INVALID',
      'current frozen candidate has no bounded authoritative Diff in the supplied runtime facts',
    )
  }
  return content
}

function evidenceSource(
  evidence: EvidenceRef,
  candidate: FrozenDeliveryCandidate,
): ResolvableDeliveryEvidenceSource {
  if (evidence.sourceRef.startsWith('runtime_event:')) {
    const eventId = evidence.sourceRef.slice('runtime_event:'.length)
    if (eventId.length === 0 || evidence.type === 'pull_request') {
      return packageError('EVIDENCE_UNRESOLVED', `EvidenceRef ${evidence.id} has no runtime source`)
    }
    return Object.freeze({
      kind: 'runtime-event',
      type: evidence.type,
      eventId,
    })
  }
  if (evidence.type === 'commit'
    && evidence.sourceRef === `git_commit:${candidate.candidateCommitId}`) {
    return Object.freeze({ kind: 'candidate-commit' })
  }
  if (evidence.type === 'diff'
    && evidence.sourceRef === `git_diff:sha256:${candidate.diffSha256}`) {
    return Object.freeze({ kind: 'candidate-diff' })
  }
  if (evidence.type === 'file') {
    const prefix = `git_file:${candidate.candidateTreeId}:`
    const marker = evidence.sourceRef.lastIndexOf('@')
    if (evidence.sourceRef.startsWith(prefix) && marker > prefix.length) {
      const encodedPath = evidence.sourceRef.slice(prefix.length, marker)
      let path: string
      try {
        path = decodeURIComponent(encodedPath)
      } catch (error) {
        return packageError(
          'EVIDENCE_UNRESOLVED',
          `EvidenceRef ${evidence.id} contains an invalid file path`,
          error,
        )
      }
      const fact = candidate.changedPaths.find(entry => entry.path === path)
      if (fact?.state === 'present'
        && evidence.sourceRef === `${prefix}${encodeURIComponent(path)}@${fact.objectId}`) {
        return Object.freeze({ kind: 'candidate-file', path })
      }
    }
  }
  return packageError(
    'EVIDENCE_UNRESOLVED',
    `EvidenceRef ${evidence.id} does not resolve to current runtime or candidate facts`,
  )
}

function resolvedEvidence(
  delivery: Delivery,
  candidate: FrozenDeliveryCandidate,
  runtimeEvents: readonly RuntimeEvent[],
): readonly EvidenceRef[] {
  const acceptance = freezeAcceptanceVerificationInput(delivery)
  const references = delivery.evidence
    .filter(reference => reference.candidateRef === candidate.candidateRef)
    .toSorted((left, right) => left.id.localeCompare(right.id))
  if (references.length === 0) {
    return packageError('EVIDENCE_UNRESOLVED', 'review package requires current candidate evidence')
  }
  for (const reference of references) {
    let resolved
    try {
      resolved = resolveDeliveryEvidence({
        delivery,
        acceptance,
        candidate,
        evidenceId: reference.id,
        stageRunId: reference.stageRunId,
        sessionBindingId: reference.sessionBindingId,
        source: evidenceSource(reference, candidate),
        runtimeEvents,
        createdAtMillis: reference.createdAtMillis,
      })
    } catch (error) {
      return packageError(
        'EVIDENCE_UNRESOLVED',
        `EvidenceRef ${reference.id} cannot be rebuilt from owning facts`,
        error,
      )
    }
    if (!equal(resolved.evidence, reference)) {
      return packageError(
        'EVIDENCE_UNRESOLVED',
        `EvidenceRef ${reference.id} changed after it was resolved`,
      )
    }
  }
  return Object.freeze(references)
}

function pullRequestBody(input: {
  readonly spec: Delivery['spec']
  readonly verdict: NonNullable<Delivery['verdict']>
  readonly candidate: FrozenDeliveryCandidate
  readonly publicationSetSha256: string
  readonly providerIdempotencyKey: string
}): string {
  const spec = input.spec
  const verdict = input.verdict
  const resultsByCriterion = new Map(verdict.criteria.map(result => [result.criterionId, result]))
  const lines = [
    `<!-- winwincode-publication:${input.providerIdempotencyKey} -->`,
    '',
    '## 交付目标',
    '',
    spec.goal,
    '',
    '## 来源',
    '',
    `Closes https://github.com/${spec.sourceRef!.repository}/issues/${String(spec.sourceRef!.number)}`,
    '',
    '## 范围',
    '',
    ...spec.scope.map(entry => `- ${entry}`),
    '',
    '## 验收结果',
    '',
    ...spec.acceptanceCriteria.map((criterion) => {
      const result = resultsByCriterion.get(criterion.id)!
      return `- ${result.verdict === 'pass' ? '[x]' : '[ ]'} ${criterion.description} — ${result.verdict} — Evidence: ${result.evidenceRefs.join(', ') || 'none'}`
    }),
    '',
    '## 冻结身份',
    '',
    `- DeliverySpec: ${spec.id} @ revision ${String(spec.revision)}`,
    `- Candidate: ${input.candidate.candidateRef}`,
    `- Diff SHA-256: ${input.candidate.diffSha256}`,
    `- DeliveryVerdict: ${verdict.id} (${verdict.status})`,
    `- Publication set: ${input.publicationSetSha256}`,
    '',
  ]
  return lines.join('\n')
}

function manifestWithoutId(
  value: Omit<StrongFlowGitHubReviewPackageManifest, 'packageId'>,
): Omit<StrongFlowGitHubReviewPackageManifest, 'packageId'> {
  return Object.freeze({
    schemaVersion: STRONGFLOW_GITHUB_REVIEW_PACKAGE_SCHEMA_VERSION,
    protocol: STRONGFLOW_GITHUB_REVIEW_PACKAGE_PROTOCOL,
    deliveryId: value.deliveryId,
    deliverySpecId: value.deliverySpecId,
    deliverySpecRevision: value.deliverySpecRevision,
    sourceRef: value.sourceRef,
    publicationTarget: value.publicationTarget,
    candidateRef: value.candidateRef,
    deliveryVerdictId: value.deliveryVerdictId,
    planReviewSetSha256: value.planReviewSetSha256,
    publicationSetSha256: value.publicationSetSha256,
    providerIdempotencyKey: value.providerIdempotencyKey,
    generatedFromMillis: value.generatedFromMillis,
    files: value.files,
    dryRun: value.dryRun,
  })
}

function packageIdentity(
  value: Omit<StrongFlowGitHubReviewPackageManifest, 'packageId'>,
): string {
  return `github-review-package:sha256:${digest(canonicalJson(manifestWithoutId(value)))}`
}

function planReviewSetDigest(context: StrongFlowPlanReviewContext): string {
  return structuredDigest(Object.freeze({
    schemaVersion: context.schemaVersion,
    protocol: context.protocol,
    deliveryId: context.deliveryId,
    deliverySpecId: context.deliverySpecId,
    deliverySpecRevision: context.deliverySpecRevision,
    planningStageRunId: context.planningStageRunId,
    planningSessionBindingId: context.planningSessionBindingId,
    reviewStageRunId: context.reviewStageRunId,
    attentionItemId: context.attentionItemId,
    solution: context.solution,
    architectureDiagram: context.architectureDiagram,
    processDiagram: context.processDiagram,
    risks: context.risks,
    unresolvedItems: context.unresolvedItems,
    preparedAtMillis: context.preparedAtMillis,
  }))
}

function publicationSetDigest(context: StrongFlowGitHubPublicationContext): string {
  return structuredDigest(Object.freeze({
    schemaVersion: context.schemaVersion,
    protocol: context.protocol,
    deliveryId: context.deliveryId,
    deliverySpecId: context.deliverySpecId,
    deliverySpecRevision: context.deliverySpecRevision,
    sourceRef: context.sourceRef,
    publicationTarget: context.publicationTarget,
    candidateRef: context.candidateRef,
    deliveryVerdictId: context.deliveryVerdictId,
    reviewStageRunId: context.reviewStageRunId,
    attentionItemId: context.attentionItemId,
    providerIdempotencyKey: context.providerIdempotencyKey,
    preparedAtMillis: context.preparedAtMillis,
  }))
}

function providerKey(input: {
  readonly deliveryId: string
  readonly sourceRef: StrongFlowGitHubPublicationContext['sourceRef']
  readonly publicationTarget: StrongFlowGitHubPublicationContext['publicationTarget']
}): string {
  return `github:pull-request:sha256:${structuredDigest({
    deliveryId: input.deliveryId,
    sourceRef: input.sourceRef,
    publicationTarget: input.publicationTarget,
  })}`
}

/** Build deterministic local review files from current canonical and runtime facts. */
export function generateStrongFlowGitHubReviewPackage(
  input: GenerateStrongFlowGitHubReviewPackageInput,
): GeneratedStrongFlowGitHubReviewPackage {
  if (typeof input !== 'object'
    || input === null
    || !Array.isArray(input.runtimeEvents)) {
    return packageError('INVALID_INPUT', 'GitHub review package input is malformed')
  }
  let delivery: Delivery
  try {
    delivery = parseDelivery(input.delivery)
  } catch (error) {
    return packageError('INVALID_INPUT', 'GitHub review package requires a valid Delivery', error)
  }
  let planReview
  try {
    planReview = assertStrongFlowPlanReviewCurrent(delivery)
  } catch (error) {
    return packageError('PLAN_REVIEW_STALE', 'approved plan-review set is not current', error)
  }
  let publicationReview
  try {
    publicationReview = assertStrongFlowGitHubPublicationReviewCurrent(
      delivery,
      input.candidate,
      input.publicationAttentionItemId,
    )
  } catch (error) {
    return packageError(
      'PUBLICATION_REVIEW_STALE',
      'GitHub publication review set is not current',
      error,
    )
  }
  const candidate = publicationReview.candidate
  const diff = authoritativeDiff(candidate, input.runtimeEvents)
  let diagramExecution
  try {
    diagramExecution = projectStrongFlowDiagramExecution(delivery, {
      runtimeEvents: input.runtimeEvents,
      candidate,
    })
  } catch (error) {
    return packageError(
      error instanceof StrongFlowDiagramExecutionProjectionError
        ? 'CANDIDATE_DIFF_INVALID'
        : 'INVALID_INPUT',
      'candidate Diff cannot be projected onto the approved diagrams',
      error,
    )
  }
  if (diagramExecution?.state !== 'execution-finished'
    || diagramExecution.details === null
    || diagramExecution.reviewSetSha256 !== planReview.context.reviewSetSha256
    || diagramExecution.details.candidate.candidateRef !== candidate.candidateRef
    || diagramExecution.details.diffSha256 !== candidate.diffSha256) {
    return packageError(
      'CANDIDATE_DIFF_INVALID',
      'candidate Diff does not match the current approved diagram set',
    )
  }
  const evidence = resolvedEvidence(delivery, candidate, input.runtimeEvents)
  const verdict = publicationReview.verdict
  const dryRun = parseStrongFlowGitHubDryRunRecord({
    schemaVersion: STRONGFLOW_GITHUB_REVIEW_PACKAGE_SCHEMA_VERSION,
    protocol: STRONGFLOW_GITHUB_DRY_RUN_PROTOCOL,
    mode: 'dry-run',
    publicationOccurred: false,
    remoteWriteCount: 0,
    publicationSetSha256: publicationReview.context.publicationSetSha256,
    recordedAtMillis: publicationReview.context.preparedAtMillis,
  })
  const preview = parseStrongFlowGitHubPullRequestPreview({
    schemaVersion: STRONGFLOW_GITHUB_REVIEW_PACKAGE_SCHEMA_VERSION,
    protocol: STRONGFLOW_GITHUB_PR_PREVIEW_PROTOCOL,
    sourceRef: publicationReview.context.sourceRef,
    publicationTarget: publicationReview.context.publicationTarget,
    title: delivery.spec.title,
    body: pullRequestBody({
      spec: delivery.spec,
      verdict,
      candidate,
      publicationSetSha256: publicationReview.context.publicationSetSha256,
      providerIdempotencyKey: publicationReview.context.providerIdempotencyKey,
    }),
    candidateRef: candidate.candidateRef,
    deliveryVerdictId: verdict.id,
    publicationSetSha256: publicationReview.context.publicationSetSha256,
  })
  const files = Object.freeze([
    file('requirements/delivery-spec.json', 'application/json', canonicalJson(delivery.spec)),
    file('solution/solution.json', 'application/json', canonicalJson(planReview.context.solution)),
    file('solution/plan-review.json', 'application/json', canonicalJson(planReview.context)),
    file(
      'solution/plan-review-decision.json',
      'application/json',
      canonicalJson(planReview.decision),
    ),
    file(
      'diagrams/system-architecture.json',
      'application/json',
      canonicalJson(planReview.context.architectureDiagram),
    ),
    file(
      'diagrams/process-flow.json',
      'application/json',
      canonicalJson(planReview.context.processDiagram),
    ),
    file('candidate/candidate.json', 'application/json', canonicalJson(candidate)),
    file('candidate/candidate.diff', 'text/x-diff', diff),
    file('verification/evidence-refs.json', 'application/json', canonicalJson(evidence)),
    file(
      'verification/criterion-results.json',
      'application/json',
      canonicalJson(verdict.criteria),
    ),
    file('verification/delivery-verdict.json', 'application/json', canonicalJson(verdict)),
    file(
      'github/publication-review.json',
      'application/json',
      canonicalJson(publicationReview.context),
    ),
    file('github/pull-request-preview.json', 'application/json', canonicalJson(preview)),
    file('github/pull-request-preview.md', 'text/markdown', `${preview.body}\n`),
    file('github/dry-run.json', 'application/json', canonicalJson(dryRun)),
  ].toSorted((left, right) => left.path.localeCompare(right.path)))
  const metadata = Object.freeze(files.map(fileMetadata))
  const totalBytes = metadata.reduce((sum, entry) => sum + entry.bytes, 0)
  if (totalBytes > MAX_PACKAGE_BYTES) {
    return packageError('PACKAGE_INVALID', 'GitHub review package exceeds its size limit')
  }
  const unsigned = manifestWithoutId({
    schemaVersion: STRONGFLOW_GITHUB_REVIEW_PACKAGE_SCHEMA_VERSION,
    protocol: STRONGFLOW_GITHUB_REVIEW_PACKAGE_PROTOCOL,
    deliveryId: delivery.id,
    deliverySpecId: delivery.spec.id,
    deliverySpecRevision: delivery.spec.revision,
    sourceRef: publicationReview.context.sourceRef,
    publicationTarget: publicationReview.context.publicationTarget,
    candidateRef: candidate.candidateRef,
    deliveryVerdictId: verdict.id,
    planReviewSetSha256: planReview.context.reviewSetSha256,
    publicationSetSha256: publicationReview.context.publicationSetSha256,
    providerIdempotencyKey: publicationReview.context.providerIdempotencyKey,
    generatedFromMillis: publicationReview.context.preparedAtMillis,
    files: metadata,
    dryRun,
  })
  const manifest = parseStrongFlowGitHubReviewPackageManifest({
    ...unsigned,
    packageId: packageIdentity(unsigned),
  })
  return verifyStrongFlowGitHubReviewPackage(immutable({
    schemaVersion: STRONGFLOW_GITHUB_REVIEW_PACKAGE_SCHEMA_VERSION,
    manifest,
    preview,
    files,
  }))
}

function contentByPath(
  reviewPackage: GeneratedStrongFlowGitHubReviewPackage,
): ReadonlyMap<string, StrongFlowGitHubReviewPackageContent> {
  return new Map(reviewPackage.files.map(entry => [entry.path, entry]))
}

/** Verify file hashes and cross-file identities without consulting external systems. */
export function verifyStrongFlowGitHubReviewPackage(
  value: unknown,
): GeneratedStrongFlowGitHubReviewPackage {
  if (!isRecord(value)
    || !exactKeys(value, ['schemaVersion', 'manifest', 'preview', 'files'])
    || value.schemaVersion !== STRONGFLOW_GITHUB_REVIEW_PACKAGE_SCHEMA_VERSION
    || !Array.isArray(value.files)) {
    return packageError('PACKAGE_INVALID', 'GitHub review package has an unexpected shape')
  }
  const packageFiles: StrongFlowGitHubReviewPackageContent[] = []
  let totalBytes = 0
  for (const [index, rawFile] of value.files.entries()) {
    if (!isRecord(rawFile)
      || !exactKeys(rawFile, ['path', 'mediaType', 'content'])
      || typeof rawFile.path !== 'string'
      || !STRONGFLOW_GITHUB_REVIEW_PACKAGE_PATHS.includes(
        rawFile.path as StrongFlowGitHubReviewPackagePath,
      )
      || typeof rawFile.mediaType !== 'string'
      || !STRONGFLOW_GITHUB_REVIEW_PACKAGE_MEDIA_TYPES.includes(
        rawFile.mediaType as StrongFlowGitHubReviewPackageMediaType,
      )
      || typeof rawFile.content !== 'string') {
      return packageError(
        'PACKAGE_INVALID',
        `GitHub review package file ${String(index)} has an unexpected shape`,
      )
    }
    totalBytes += Buffer.byteLength(rawFile.content)
    if (totalBytes > MAX_PACKAGE_BYTES) {
      return packageError('PACKAGE_INVALID', 'GitHub review package exceeds its size limit')
    }
    packageFiles.push(Object.freeze({
      path: rawFile.path as StrongFlowGitHubReviewPackagePath,
      mediaType: rawFile.mediaType as StrongFlowGitHubReviewPackageMediaType,
      content: rawFile.content,
    }))
  }
  let manifest: StrongFlowGitHubReviewPackageManifest
  try {
    manifest = parseStrongFlowGitHubReviewPackageManifest(value.manifest)
  } catch (error) {
    return packageError('PACKAGE_INVALID', 'GitHub review package manifest is invalid', error)
  }
  const files = contentByPath({
    schemaVersion: STRONGFLOW_GITHUB_REVIEW_PACKAGE_SCHEMA_VERSION,
    manifest,
    preview: value.preview as StrongFlowGitHubPullRequestPreview,
    files: packageFiles,
  })
  const expectedPaths = [...STRONGFLOW_GITHUB_REVIEW_PACKAGE_PATHS].sort()
  if (files.size !== expectedPaths.length
    || expectedPaths.some(path => !files.has(path))
    || packageFiles.some(entry => (
      !STRONGFLOW_GITHUB_REVIEW_PACKAGE_PATHS.includes(entry.path)
    ))) {
    return packageError('PACKAGE_INVALID', 'GitHub review package file set is incomplete')
  }
  const actualMetadata = Object.freeze(packageFiles
    .map(fileMetadata)
    .toSorted((left, right) => left.path.localeCompare(right.path)))
  if (!equal(manifest.files, actualMetadata)) {
    return packageError('PACKAGE_INVALID', 'GitHub review package file hash or size changed')
  }
  const unsigned = manifestWithoutId(manifest)
  if (manifest.packageId !== packageIdentity(unsigned)) {
    return packageError('PACKAGE_INVALID', 'GitHub review package identity changed')
  }
  try {
    const spec = parseDeliverySpec(JSON.parse(files.get('requirements/delivery-spec.json')!.content))
    const solution = parseStrongFlowPlanReviewSolution(
      JSON.parse(files.get('solution/solution.json')!.content),
    )
    const planReview = parseStrongFlowPlanReviewContext(
      JSON.parse(files.get('solution/plan-review.json')!.content),
    )
    const planDecision = parseStrongFlowPlanReviewDecision(
      JSON.parse(files.get('solution/plan-review-decision.json')!.content),
    )
    const architecture = parseStrongFlowPlanReviewDiagram(
      JSON.parse(files.get('diagrams/system-architecture.json')!.content),
    )
    const process = parseStrongFlowPlanReviewDiagram(
      JSON.parse(files.get('diagrams/process-flow.json')!.content),
    )
    const candidate = parseFrozenDeliveryCandidate(
      JSON.parse(files.get('candidate/candidate.json')!.content),
    )
    const evidenceInput = JSON.parse(files.get('verification/evidence-refs.json')!.content)
    const criteriaInput = JSON.parse(files.get('verification/criterion-results.json')!.content)
    if (!Array.isArray(evidenceInput) || !Array.isArray(criteriaInput)) {
      return packageError('PACKAGE_INVALID', 'verification files must contain arrays')
    }
    const evidence = evidenceInput.map((entry, index) => parseEvidenceRef(
      entry,
      `githubReviewPackage.evidence[${String(index)}]`,
    ))
    const criteria = criteriaInput.map((entry, index) => parseCriterionResult(
      entry,
      `githubReviewPackage.criteria[${String(index)}]`,
    ))
    const verdict = parseDeliveryVerdict(
      JSON.parse(files.get('verification/delivery-verdict.json')!.content),
    )
    const publicationReview = parseStrongFlowGitHubPublicationContext(
      JSON.parse(files.get('github/publication-review.json')!.content),
    )
    const preview = parseStrongFlowGitHubPullRequestPreview(
      JSON.parse(files.get('github/pull-request-preview.json')!.content),
    )
    const dryRun = parseStrongFlowGitHubDryRunRecord(
      JSON.parse(files.get('github/dry-run.json')!.content),
    )
    const diff = files.get('candidate/candidate.diff')!.content
    const expectedPreview = parseStrongFlowGitHubPullRequestPreview({
      schemaVersion: STRONGFLOW_GITHUB_REVIEW_PACKAGE_SCHEMA_VERSION,
      protocol: STRONGFLOW_GITHUB_PR_PREVIEW_PROTOCOL,
      sourceRef: spec.sourceRef,
      publicationTarget: spec.publicationTarget,
      title: spec.title,
      body: pullRequestBody({
        spec,
        verdict,
        candidate,
        publicationSetSha256: publicationReview.publicationSetSha256,
        providerIdempotencyKey: publicationReview.providerIdempotencyKey,
      }),
      candidateRef: candidate.candidateRef,
      deliveryVerdictId: verdict.id,
      publicationSetSha256: publicationReview.publicationSetSha256,
    })
    if (spec.deliveryId !== manifest.deliveryId
      || spec.id !== manifest.deliverySpecId
      || spec.revision !== manifest.deliverySpecRevision
      || spec.sourceRef === null
      || spec.publicationTarget === null
      || !equal(spec.sourceRef, manifest.sourceRef)
      || !equal(spec.publicationTarget, manifest.publicationTarget)
      || planReview.deliveryId !== manifest.deliveryId
      || planReview.deliverySpecId !== manifest.deliverySpecId
      || planReview.deliverySpecRevision !== manifest.deliverySpecRevision
      || planReview.reviewSetSha256 !== manifest.planReviewSetSha256
      || planReview.reviewSetSha256 !== planReviewSetDigest(planReview)
      || planDecision.action !== 'approve'
      || planDecision.deliveryId !== planReview.deliveryId
      || planDecision.deliverySpecId !== planReview.deliverySpecId
      || planDecision.deliverySpecRevision !== planReview.deliverySpecRevision
      || planDecision.reviewStageRunId !== planReview.reviewStageRunId
      || planDecision.attentionItemId !== planReview.attentionItemId
      || planDecision.reviewSetSha256 !== planReview.reviewSetSha256
      || !equal(planReview.solution, solution)
      || !equal(planReview.architectureDiagram, architecture)
      || !equal(planReview.processDiagram, process)
      || candidate.candidateRef !== manifest.candidateRef
      || candidate.deliveryId !== manifest.deliveryId
      || candidate.deliverySpecId !== manifest.deliverySpecId
      || candidate.deliverySpecRevision !== manifest.deliverySpecRevision
      || candidate.repositoryKind !== spec.repository.kind
      || candidate.repositoryLocator !== spec.repository.locator
      || candidate.baseRevision !== spec.baseRevision
      || candidate.diffSha256 !== digest(diff)
      || verdict.id !== manifest.deliveryVerdictId
      || verdict.deliveryId !== manifest.deliveryId
      || verdict.deliverySpecId !== manifest.deliverySpecId
      || verdict.candidateRef !== manifest.candidateRef
      || !equal(verdict.criteria, criteria)
      || evidence.length === 0
      || evidence.some(reference => (
        reference.deliveryId !== manifest.deliveryId
        || reference.deliverySpecId !== manifest.deliverySpecId
        || reference.deliverySpecRevision !== manifest.deliverySpecRevision
        || reference.candidateRef !== manifest.candidateRef
      ))
      || criteria.some(result => (
        result.deliveryId !== manifest.deliveryId
        || result.deliverySpecId !== manifest.deliverySpecId
        || result.candidateRef !== manifest.candidateRef
        || !spec.acceptanceCriteria.some(criterion => criterion.id === result.criterionId)
      ))
      || verdict.criteria.some(result => result.evidenceRefs.some(id => (
        !evidence.some(reference => reference.id === id)
      )))
      || architecture.kind !== 'system-architecture'
      || process.kind !== 'process-flow'
      || solution.id.length === 0
      || publicationReview.deliveryId !== manifest.deliveryId
      || publicationReview.deliverySpecId !== manifest.deliverySpecId
      || publicationReview.deliverySpecRevision !== manifest.deliverySpecRevision
      || !equal(publicationReview.sourceRef, manifest.sourceRef)
      || !equal(publicationReview.publicationTarget, manifest.publicationTarget)
      || publicationReview.candidateRef !== manifest.candidateRef
      || publicationReview.deliveryVerdictId !== manifest.deliveryVerdictId
      || publicationReview.publicationSetSha256 !== manifest.publicationSetSha256
      || publicationReview.publicationSetSha256 !== publicationSetDigest(publicationReview)
      || publicationReview.providerIdempotencyKey !== manifest.providerIdempotencyKey
      || publicationReview.providerIdempotencyKey !== providerKey({
        deliveryId: manifest.deliveryId,
        sourceRef: manifest.sourceRef,
        publicationTarget: manifest.publicationTarget,
      })
      || publicationReview.preparedAtMillis !== manifest.generatedFromMillis
      || !equal(preview, parseStrongFlowGitHubPullRequestPreview(value.preview))
      || !equal(preview, expectedPreview)
      || preview.candidateRef !== manifest.candidateRef
      || preview.deliveryVerdictId !== manifest.deliveryVerdictId
      || preview.publicationSetSha256 !== manifest.publicationSetSha256
      || dryRun.publicationSetSha256 !== manifest.publicationSetSha256
      || dryRun.recordedAtMillis !== publicationReview.preparedAtMillis
      || !equal(dryRun, manifest.dryRun)
      || files.get('github/pull-request-preview.md')!.content !== `${preview.body}\n`) {
      return packageError('PACKAGE_INVALID', 'GitHub review package cross-file identity changed')
    }
  } catch (error) {
    if (error instanceof StrongFlowGitHubReviewPackageError) throw error
    return packageError('PACKAGE_INVALID', 'GitHub review package content is invalid', error)
  }
  return immutable({
    schemaVersion: STRONGFLOW_GITHUB_REVIEW_PACKAGE_SCHEMA_VERSION,
    manifest,
    preview: parseStrongFlowGitHubPullRequestPreview(value.preview),
    files: packageFiles.toSorted((left, right) => left.path.localeCompare(right.path)),
  })
}

async function pathExists(path: string): Promise<boolean> {
  try {
    await lstat(path)
    return true
  } catch (error) {
    if (typeof error === 'object' && error !== null && 'code' in error && error.code === 'ENOENT') {
      return false
    }
    throw error
  }
}

async function listedFiles(root: string, current = root): Promise<readonly string[]> {
  const entries = await readdir(current, { withFileTypes: true })
  const paths: string[] = []
  for (const entry of entries) {
    const path = join(current, entry.name)
    if (entry.isDirectory()) paths.push(...await listedFiles(root, path))
    else if (entry.isFile()) paths.push(relative(root, path).replaceAll('\\', '/'))
    else return packageError('OUTPUT_CONFLICT', 'review package contains a non-file entry')
  }
  return Object.freeze(paths.sort())
}

/** Read and verify one previously written local package. */
export async function readStrongFlowGitHubReviewPackage(
  directoryInput: string,
): Promise<GeneratedStrongFlowGitHubReviewPackage> {
  const directory = resolve(directoryInput)
  try {
    const stat = await lstat(directory)
    if (!stat.isDirectory()) return packageError('OUTPUT_CONFLICT', 'review package is not a directory')
    const paths = await listedFiles(directory)
    const expected = ['manifest.json', ...STRONGFLOW_GITHUB_REVIEW_PACKAGE_PATHS].sort()
    if (!equal(paths, expected)) {
      return packageError('OUTPUT_CONFLICT', 'review package directory has an unexpected file set')
    }
    const manifest = parseStrongFlowGitHubReviewPackageManifest(
      JSON.parse(await readFile(join(directory, 'manifest.json'), 'utf8')),
    )
    const files = await Promise.all(manifest.files.map(async metadata => Object.freeze({
      path: metadata.path as StrongFlowGitHubReviewPackagePath,
      mediaType: metadata.mediaType,
      content: await readFile(join(directory, metadata.path), 'utf8'),
    })))
    const preview = parseStrongFlowGitHubPullRequestPreview(
      JSON.parse(files.find(entry => entry.path === 'github/pull-request-preview.json')!.content),
    )
    return verifyStrongFlowGitHubReviewPackage({
      schemaVersion: STRONGFLOW_GITHUB_REVIEW_PACKAGE_SCHEMA_VERSION,
      manifest,
      preview,
      files,
    })
  } catch (error) {
    if (error instanceof StrongFlowGitHubReviewPackageError) throw error
    return packageError('OUTPUT_IO_ERROR', 'review package could not be read', error)
  }
}

/** Atomically write local review files; this function has no remote provider capability. */
export async function writeStrongFlowGitHubReviewPackage(
  input: WriteStrongFlowGitHubReviewPackageInput,
): Promise<WrittenStrongFlowGitHubReviewPackage> {
  const reviewPackage = verifyStrongFlowGitHubReviewPackage(input.reviewPackage)
  if (typeof input.outputDirectory !== 'string' || input.outputDirectory.length === 0) {
    return packageError('INVALID_INPUT', 'review package output directory is invalid')
  }
  const directory = resolve(input.outputDirectory)
  if (await pathExists(directory)) {
    const existing = await readStrongFlowGitHubReviewPackage(directory)
    if (existing.manifest.packageId !== reviewPackage.manifest.packageId) {
      return packageError('OUTPUT_CONFLICT', 'output directory contains another review package')
    }
    return Object.freeze({ directory, manifest: existing.manifest, reused: true })
  }
  await mkdir(dirname(directory), { recursive: true, mode: 0o700 })
  const temporary = `${directory}.pending-${randomUUID()}`
  try {
    await mkdir(temporary, { mode: 0o700 })
    for (const entry of reviewPackage.files) {
      const path = join(temporary, entry.path)
      await mkdir(dirname(path), { recursive: true, mode: 0o700 })
      await writeFile(path, entry.content, { encoding: 'utf8', flag: 'wx', mode: 0o600 })
    }
    await writeFile(
      join(temporary, 'manifest.json'),
      canonicalJson(reviewPackage.manifest),
      { encoding: 'utf8', flag: 'wx', mode: 0o600 },
    )
    await rename(temporary, directory)
    return Object.freeze({
      directory,
      manifest: reviewPackage.manifest,
      reused: false,
    })
  } catch (error) {
    await rm(temporary, { recursive: true, force: true })
    if (await pathExists(directory)) {
      const existing = await readStrongFlowGitHubReviewPackage(directory)
      if (existing.manifest.packageId === reviewPackage.manifest.packageId) {
        return Object.freeze({ directory, manifest: existing.manifest, reused: true })
      }
      return packageError('OUTPUT_CONFLICT', 'output directory was published by another package')
    }
    if (error instanceof StrongFlowGitHubReviewPackageError) throw error
    return packageError('OUTPUT_IO_ERROR', 'review package could not be written', error)
  }
}
