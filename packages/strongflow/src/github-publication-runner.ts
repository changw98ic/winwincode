import {
  parseAttentionItem,
  parseDelivery,
  type AttentionItem,
  type Delivery,
  type FrozenDeliveryCandidate,
} from '@winwincode/contracts'

import { assertFrozenDeliveryCandidateCurrent } from './candidate-evidence.js'
import { containsRawCredentialMaterial } from './credential-boundary.js'
import {
  type GeneratedStrongFlowGitHubReviewPackage,
  verifyStrongFlowGitHubReviewPackage,
} from './github-review-package.js'
import {
  StrongFlowGitHubPublicationJournal,
  type StrongFlowGitHubPublicationJournalProjection,
  type StrongFlowGitHubPublicationStepProjection,
} from './github-publication-journal.js'
import {
  StrongFlowGitHubPublicationProviderError,
  buildStrongFlowGitHubProviderOperations,
  parseStrongFlowGitHubProviderMutation,
  parseStrongFlowGitHubProviderObservation,
  type StrongFlowGitHubProviderMutation,
  type StrongFlowGitHubProviderObservation,
  type StrongFlowGitHubProviderOperation,
  type StrongFlowGitHubPublicationProvider,
} from './github-publication-provider.js'
import {
  assertStrongFlowGitHubPublicationCurrent,
  assertStrongFlowGitHubPublicationReviewCurrent,
} from './github-publication.js'

export const STRONGFLOW_GITHUB_PUBLICATION_RUNNER_SCHEMA_VERSION = 1 as const

export type StrongFlowGitHubPublicationMode = 'dry-run' | 'live'

export interface RunStrongFlowGitHubPublicationInput {
  readonly home: string
  readonly mode?: StrongFlowGitHubPublicationMode
  readonly delivery: Delivery
  readonly candidate: FrozenDeliveryCandidate
  readonly reviewPackage: GeneratedStrongFlowGitHubReviewPackage
  readonly publicationAttentionItemId: string
  readonly provider?: StrongFlowGitHubPublicationProvider
  readonly clock?: () => number
}

export interface StrongFlowGitHubPublicationDryRunResult {
  readonly schemaVersion: typeof STRONGFLOW_GITHUB_PUBLICATION_RUNNER_SCHEMA_VERSION
  readonly mode: 'dry-run'
  readonly status: 'dry-run'
  readonly reviewPackageId: string
  readonly providerIdempotencyKey: string
  readonly publicationSetSha256: string
  readonly remoteWriteCount: 0
}

export interface StrongFlowGitHubPublicationLiveResult {
  readonly schemaVersion: typeof STRONGFLOW_GITHUB_PUBLICATION_RUNNER_SCHEMA_VERSION
  readonly mode: 'live'
  readonly status: 'succeeded' | 'failed' | 'pending'
  readonly reviewPackageId: string
  readonly providerIdempotencyKey: string
  readonly publicationSetSha256: string
  readonly confirmedRemoteWriteCount: number
  readonly journal: StrongFlowGitHubPublicationJournalProjection
}

export type StrongFlowGitHubPublicationRunResult =
  | StrongFlowGitHubPublicationDryRunResult
  | StrongFlowGitHubPublicationLiveResult

export type StrongFlowGitHubPublicationRunnerErrorCode =
  | 'INVALID_INPUT'
  | 'STALE_PUBLICATION'
  | 'LIVE_APPROVAL_REQUIRED'
  | 'PROVIDER_REQUIRED'
  | 'PROVIDER_CONTRACT_ERROR'
  | 'JOURNAL_ERROR'

export class StrongFlowGitHubPublicationRunnerError extends Error {
  readonly code: StrongFlowGitHubPublicationRunnerErrorCode

  constructor(
    code: StrongFlowGitHubPublicationRunnerErrorCode,
    message: string,
    options?: ErrorOptions,
  ) {
    super(message, options)
    this.name = 'StrongFlowGitHubPublicationRunnerError'
    this.code = code
  }
}

interface CurrentPublicationInput {
  readonly delivery: Delivery
  readonly candidate: FrozenDeliveryCandidate
  readonly attention: AttentionItem
  readonly reviewPackage: GeneratedStrongFlowGitHubReviewPackage
}

function runnerError(
  code: StrongFlowGitHubPublicationRunnerErrorCode,
  message: string,
  cause?: unknown,
): never {
  throw new StrongFlowGitHubPublicationRunnerError(
    code,
    message,
    cause === undefined ? undefined : { cause },
  )
}

function equal(left: unknown, right: unknown): boolean {
  return JSON.stringify(left) === JSON.stringify(right)
}

function currentInput(input: RunStrongFlowGitHubPublicationInput): CurrentPublicationInput {
  let delivery: Delivery
  let candidate: FrozenDeliveryCandidate
  let reviewPackage: GeneratedStrongFlowGitHubReviewPackage
  let attention: AttentionItem
  try {
    delivery = parseDelivery(input.delivery)
    candidate = assertFrozenDeliveryCandidateCurrent(delivery, input.candidate)
    reviewPackage = verifyStrongFlowGitHubReviewPackage(input.reviewPackage)
    attention = parseAttentionItem(
      delivery.attentionItems.find(item => item.id === input.publicationAttentionItemId),
    )
  } catch (error) {
    return runnerError('INVALID_INPUT', 'GitHub publication input is invalid', error)
  }
  if (containsRawCredentialMaterial({ delivery, candidate, reviewPackage })) {
    return runnerError('INVALID_INPUT', 'GitHub publication facts contain raw credential material')
  }
  const manifest = reviewPackage.manifest
  if (manifest.deliveryId !== delivery.id
    || manifest.deliverySpecId !== delivery.spec.id
    || manifest.deliverySpecRevision !== delivery.spec.revision
    || manifest.candidateRef !== candidate.candidateRef
    || manifest.deliveryVerdictId !== delivery.verdict?.id
    || !equal(manifest.sourceRef, delivery.spec.sourceRef)
    || !equal(manifest.publicationTarget, delivery.spec.publicationTarget)) {
    return runnerError(
      'STALE_PUBLICATION',
      'review package does not match the current Delivery, candidate, verdict, or destination',
    )
  }
  return Object.freeze({ delivery, candidate, attention, reviewPackage })
}

function clockValue(clock: () => number): number {
  const value = clock()
  if (!Number.isSafeInteger(value) || value < 0 || Object.is(value, -0)) {
    return runnerError('INVALID_INPUT', 'publication clock returned an invalid timestamp')
  }
  return value
}

function liveStatus(
  projection: StrongFlowGitHubPublicationJournalProjection,
): StrongFlowGitHubPublicationLiveResult['status'] {
  if (projection.status === 'succeeded') return 'succeeded'
  if (projection.status === 'failed') return 'failed'
  return 'pending'
}

async function appendLookup(
  journal: StrongFlowGitHubPublicationJournal,
  operation: StrongFlowGitHubProviderOperation,
  observation: StrongFlowGitHubProviderObservation,
  clock: () => number,
): Promise<StrongFlowGitHubPublicationJournalProjection> {
  const outcome = observation.state === 'found'
    ? observation.requestSha256 === operation.requestSha256
      ? 'lookup-found-current' as const
      : 'lookup-found-stale' as const
    : observation.state === 'absent'
      ? 'lookup-absent' as const
      : observation.state === 'unknown'
        ? 'lookup-unknown' as const
        : 'lookup-conflict' as const
  return journal.append({
    operation,
    outcome,
    code: observation.state === 'unknown' || observation.state === 'conflict'
      ? observation.code
      : null,
    resourceRef: observation.state === 'found' ? observation.resourceRef : null,
    remoteWritePerformed: null,
    recordedAtMillis: clockValue(clock),
  })
}

async function appendMutation(
  journal: StrongFlowGitHubPublicationJournal,
  operation: StrongFlowGitHubProviderOperation,
  mutation: StrongFlowGitHubProviderMutation,
  clock: () => number,
): Promise<StrongFlowGitHubPublicationJournalProjection> {
  return journal.append({
    operation,
    outcome: mutation.state === 'applied'
      ? 'apply-applied'
      : mutation.state === 'unknown'
        ? 'apply-unknown'
        : 'apply-rejected',
    code: mutation.state === 'applied' ? null : mutation.code,
    resourceRef: mutation.state === 'applied' ? mutation.resourceRef : null,
    remoteWritePerformed: mutation.state === 'applied'
      ? mutation.remoteWritePerformed
      : null,
    recordedAtMillis: clockValue(clock),
  })
}

function currentStep(
  projection: StrongFlowGitHubPublicationJournalProjection,
  operation: StrongFlowGitHubProviderOperation,
): StrongFlowGitHubPublicationStepProjection {
  const step = projection.steps.find(entry => entry.operationKey === operation.operationKey)
  if (step === undefined || step.requestSha256 !== operation.requestSha256) {
    return runnerError('JOURNAL_ERROR', 'publication journal lost an intended operation')
  }
  return step
}

async function observe(
  provider: StrongFlowGitHubPublicationProvider,
  operation: StrongFlowGitHubProviderOperation,
): Promise<StrongFlowGitHubProviderObservation> {
  let value: unknown
  try {
    value = await provider.lookup(operation)
  } catch {
    return Object.freeze({
      state: 'unknown',
      operationKey: operation.operationKey,
      code: 'lookup-outcome-unknown',
    })
  }
  try {
    return parseStrongFlowGitHubProviderObservation(operation, value)
  } catch (error) {
    if (error instanceof StrongFlowGitHubPublicationProviderError) {
      return runnerError(
        'PROVIDER_CONTRACT_ERROR',
        'GitHub provider returned an invalid lookup result',
        error,
      )
    }
    throw error
  }
}

async function mutate(
  provider: StrongFlowGitHubPublicationProvider,
  operation: StrongFlowGitHubProviderOperation,
): Promise<StrongFlowGitHubProviderMutation> {
  let value: unknown
  try {
    value = await provider.apply(operation)
  } catch {
    return Object.freeze({
      state: 'unknown',
      operationKey: operation.operationKey,
      code: 'apply-outcome-unknown',
    })
  }
  try {
    return parseStrongFlowGitHubProviderMutation(operation, value)
  } catch (error) {
    if (error instanceof StrongFlowGitHubPublicationProviderError) {
      return Object.freeze({
        state: 'unknown',
        operationKey: operation.operationKey,
        code: 'apply-result-invalid',
      })
    }
    throw error
  }
}

async function runOperation(
  journal: StrongFlowGitHubPublicationJournal,
  provider: StrongFlowGitHubPublicationProvider,
  operation: StrongFlowGitHubProviderOperation,
  clock: () => number,
): Promise<StrongFlowGitHubPublicationJournalProjection> {
  let projection = await journal.read()
  if (currentStep(projection, operation).state === 'succeeded') return projection
  const observation = await observe(provider, operation)
  projection = await appendLookup(journal, operation, observation, clock)
  if (observation.state === 'found'
    && observation.requestSha256 === operation.requestSha256) return projection
  if (observation.state === 'unknown' || observation.state === 'conflict') return projection
  projection = await journal.append({
    operation,
    outcome: 'apply-intent',
    code: null,
    resourceRef: null,
    remoteWritePerformed: null,
    recordedAtMillis: clockValue(clock),
  })
  const mutation = await mutate(provider, operation)
  return appendMutation(journal, operation, mutation, clock)
}

/**
 * Dry-run is the default and never touches the provider. Live mode requires an
 * exact resolved human decision and records all intent before provider writes.
 */
export async function runStrongFlowGitHubPublication(
  input: RunStrongFlowGitHubPublicationInput,
): Promise<StrongFlowGitHubPublicationRunResult> {
  if (typeof input !== 'object' || input === null) {
    return runnerError('INVALID_INPUT', 'GitHub publication input is malformed')
  }
  const mode = input.mode ?? 'dry-run'
  if (mode !== 'dry-run' && mode !== 'live') {
    return runnerError('INVALID_INPUT', 'GitHub publication mode is invalid')
  }
  const current = currentInput(input)
  const manifest = current.reviewPackage.manifest
  if (mode === 'dry-run') {
    try {
      assertStrongFlowGitHubPublicationReviewCurrent(
        current.delivery,
        current.candidate,
        current.attention.id,
      )
    } catch (error) {
      return runnerError('STALE_PUBLICATION', 'dry-run review set is no longer current', error)
    }
    return Object.freeze({
      schemaVersion: STRONGFLOW_GITHUB_PUBLICATION_RUNNER_SCHEMA_VERSION,
      mode: 'dry-run',
      status: 'dry-run',
      reviewPackageId: manifest.packageId,
      providerIdempotencyKey: manifest.providerIdempotencyKey,
      publicationSetSha256: manifest.publicationSetSha256,
      remoteWriteCount: 0,
    })
  }
  let approved
  try {
    approved = assertStrongFlowGitHubPublicationCurrent(
      current.delivery,
      current.candidate,
      current.attention,
    )
  } catch (error) {
    return runnerError(
      'LIVE_APPROVAL_REQUIRED',
      'live GitHub publication requires the exact current human approval',
      error,
    )
  }
  if (input.provider === undefined
    || typeof input.provider.lookup !== 'function'
    || typeof input.provider.apply !== 'function') {
    return runnerError('PROVIDER_REQUIRED', 'live GitHub publication requires a provider adapter')
  }
  if (approved.context.publicationSetSha256 !== manifest.publicationSetSha256
    || approved.context.providerIdempotencyKey !== manifest.providerIdempotencyKey) {
    return runnerError('STALE_PUBLICATION', 'approved publication differs from review package')
  }
  let operations: readonly StrongFlowGitHubProviderOperation[]
  try {
    operations = buildStrongFlowGitHubProviderOperations(
      current.reviewPackage,
      current.candidate,
    )
  } catch (error) {
    return runnerError('INVALID_INPUT', 'GitHub provider intent could not be derived', error)
  }
  let journal: StrongFlowGitHubPublicationJournal
  try {
    journal = await StrongFlowGitHubPublicationJournal.createOrOpen({
      home: input.home,
      providerIdempotencyKey: manifest.providerIdempotencyKey,
      reviewPackageId: manifest.packageId,
      deliveryId: manifest.deliveryId,
      deliverySpecId: manifest.deliverySpecId,
      deliverySpecRevision: manifest.deliverySpecRevision,
      candidateRef: manifest.candidateRef,
      deliveryVerdictId: manifest.deliveryVerdictId,
      publicationSetSha256: manifest.publicationSetSha256,
      operations,
      createdAtMillis: approved.approvedAtMillis,
    })
  } catch (error) {
    return runnerError('JOURNAL_ERROR', 'GitHub publication intent could not be opened', error)
  }
  const clock = input.clock ?? Date.now
  let projection: StrongFlowGitHubPublicationJournalProjection
  try {
    projection = await journal.read()
  } catch (error) {
    return runnerError('JOURNAL_ERROR', 'GitHub publication progress could not be read', error)
  }
  for (const operation of operations) {
    try {
      projection = await runOperation(journal, input.provider, operation, clock)
    } catch (error) {
      if (error instanceof StrongFlowGitHubPublicationRunnerError) throw error
      return runnerError('JOURNAL_ERROR', 'GitHub publication progress could not be stored', error)
    }
    const step = currentStep(projection, operation)
    if (step.state !== 'succeeded') break
  }
  return Object.freeze({
    schemaVersion: STRONGFLOW_GITHUB_PUBLICATION_RUNNER_SCHEMA_VERSION,
    mode: 'live',
    status: liveStatus(projection),
    reviewPackageId: manifest.packageId,
    providerIdempotencyKey: manifest.providerIdempotencyKey,
    publicationSetSha256: manifest.publicationSetSha256,
    confirmedRemoteWriteCount: projection.confirmedRemoteWriteCount,
    journal: projection,
  })
}
