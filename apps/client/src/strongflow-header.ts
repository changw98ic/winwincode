// SPDX-License-Identifier: Apache-2.0

import type {
  StrongFlowProjection,
  StrongFlowRealtimeStatus,
  StrongFlowViewModelState,
} from './strongflow-view-model.js'
import { strongFlowElement } from './strongflow-rendering.js'

/**
 * First-screen header for StrongFlow (UI-306).
 *
 * The selectors below answer three questions in human words — what is happening
 * now, what is blocking it, and what the user should do next — straight from the
 * already-delivered Delivery snapshot. Technical identities stay inside the
 * collapsible identity card, where each kind of identity gets its own labeled
 * row built only from exact SessionBinding, StageRun, runtime, and Candidate
 * facts. Absent facts are reported as "Not reported" instead of being guessed
 * from a similar-looking identifier.
 */

export type StrongFlowNextStepCategory =
  | 'active'
  | 'completed'
  | 'draft'
  | 'failed'
  | 'verifying'
  | 'waiting-approval'
  | 'waiting-input'

export interface StrongFlowCurrentRun {
  readonly role: string | null
  readonly attempt: number | null
  readonly phase: string | null
  readonly status: string | null
}

export interface StrongFlowNextStep {
  readonly category: StrongFlowNextStepCategory
  readonly statusLabel: string
  readonly reason: string | null
  readonly nextStep: string
  readonly currentRun: StrongFlowCurrentRun | null
}

export const STRONGFLOW_IDENTITY_NOT_REPORTED = 'Not reported'
const CANDIDATE_NONE = 'None frozen yet'

const IDENTITY_TERMS = Object.freeze([
  'ProductSession',
  'StageRun',
  'Attempt',
  'ExecutionJob',
  'Worker',
  'WorkerSession',
  'CodexThread',
  'Model route',
  'Candidate',
  'Lease',
  'Events connection',
])

export interface StrongFlowExecutionIdentity {
  readonly productSessionId: string | null
  readonly stageRunId: string | null
  readonly attempt: number | null
  readonly executionJobId: string | null
  readonly workerId: string | null
  readonly workerSessionId: string | null
  readonly codexThreadId: string | null
  readonly modelRoute: string | null
  readonly candidateRef: string | null
  readonly leaseId: string | null
  readonly leaseHeld: boolean
  readonly connection: string
}

export interface StrongFlowIdentityRow {
  readonly term: string
  readonly value: string
}

type AttentionRecord = StrongFlowProjection['attention'][number]

function openAttentionOf(projection: StrongFlowProjection): AttentionRecord | null {
  const open = projection.attention.filter(item => item.status === 'open')
  return open.find(item => item.blocking) ?? open[0] ?? null
}

function currentRunOf(projection: StrongFlowProjection): StrongFlowCurrentRun | null {
  const stage = projection.stage
  if (stage === undefined || stage === null) return null
  return Object.freeze({
    role: typeof stage.role === 'string' ? stage.role : null,
    attempt: typeof stage.attempt === 'number' ? stage.attempt : null,
    phase: typeof stage.stage === 'string' ? stage.stage : null,
    status: typeof stage.status === 'string' ? stage.status : null,
  })
}

interface StrongFlowFailure {
  readonly reason: string
  readonly nextStep: string
}

function failureOf(projection: StrongFlowProjection): StrongFlowFailure | null {
  const verdict = projection.verdict
  if (verdict !== null && verdict.status === 'fail') {
    const findings = verdict.unresolvedFindings.length
    return {
      reason: findings === 1
        ? 'The final verdict failed with 1 unresolved finding.'
        : `The final verdict failed with ${String(findings)} unresolved findings.`,
      nextStep: 'Review each unresolved finding, then resolve or re-run the Delivery.',
    }
  }
  if (verdict !== null && verdict.status === 'infra_error') {
    return {
      reason: 'Verification stopped with an infrastructure error.',
      nextStep: 'Retry verification once the infrastructure error is resolved.',
    }
  }
  const failedStage = projection.delivery.stages.find(item => item.status === 'failed')
  if (failedStage !== undefined) {
    return {
      reason: `The ${failedStage.role} run failed.`,
      nextStep: 'Review the failed run and resolve the failure before retrying this Delivery.',
    }
  }
  if (projection.delivery.publication?.state === 'failed') {
    return {
      reason: 'Publication of this Delivery failed.',
      nextStep: 'Review the publication target and retry before continuing.',
    }
  }
  return null
}

function publicationReason(projection: StrongFlowProjection): string | null {
  const publication = projection.delivery.publication
  if (publication === null) return null
  switch (publication.state) {
    case 'published': return 'The approved candidate was published.'
    case 'publishing': return 'Publication of the approved candidate is in progress.'
    case 'pending': return 'Publication of the approved candidate is pending.'
    case 'cancelled': return 'Publication of this Delivery was cancelled.'
    default: return null
  }
}

function runSummary(run: StrongFlowCurrentRun | null): string {
  if (run === null) return ''
  const parts = [
    run.role,
    run.attempt === null ? null : `attempt ${String(run.attempt)}`,
    run.status,
  ].filter((value): value is string => value !== null && value.length > 0)
  return parts.join(' · ')
}

/**
 * Deterministic next-step selector over one exact Delivery snapshot. Every
 * answer cites the precise blocking fact; nothing is inferred beyond the
 * delivered projection.
 */
export function strongFlowNextStep(
  projection: StrongFlowProjection,
): StrongFlowNextStep {
  const currentRun = currentRunOf(projection)
  const failure = failureOf(projection)
  if (failure !== null) {
    return Object.freeze({
      category: 'failed',
      statusLabel: 'Failed',
      reason: failure.reason,
      nextStep: failure.nextStep,
      currentRun,
    })
  }
  const status = projection.delivery.status
  if (status === 'delivered') {
    return Object.freeze({
      category: 'completed',
      statusLabel: 'Completed',
      reason: publicationReason(projection) ?? 'This Delivery reached its goal.',
      nextStep: 'This Delivery is complete. Review the published result or start a new Delivery.',
      currentRun,
    })
  }
  const attention = openAttentionOf(projection)
  if (attention !== null && attention.type === 'delivery_approval') {
    return Object.freeze({
      category: 'waiting-approval',
      statusLabel: 'Waiting for your approval',
      reason: `"${attention.title}" asks for your approval.`,
      nextStep: 'Approve the Delivery to continue.',
      currentRun,
    })
  }
  if (attention !== null) {
    return Object.freeze({
      category: 'waiting-input',
      statusLabel: 'Waiting for your input',
      reason: `"${attention.title}" is open and blocks this Delivery.`,
      nextStep: 'Resolve the open Attention records in Review actions to continue.',
      currentRun,
    })
  }
  if (status === 'plan-review') {
    const reviewStatus = projection.solutionReview?.reviewStatus ?? null
    if (reviewStatus === 'approved') {
      return Object.freeze({
        category: 'waiting-approval',
        statusLabel: 'Waiting for your approval',
        reason: 'The approved solution is waiting for its task breakdown.',
        nextStep: 'Approve the task breakdown of the approved solution to continue.',
        currentRun,
      })
    }
    return Object.freeze({
      category: 'waiting-approval',
      statusLabel: 'Waiting for your approval',
      reason: 'The proposed solution is waiting for your review.',
      nextStep: 'Review the proposed solution and approve it or request changes.',
      currentRun,
    })
  }
  if (status === 'ready-to-deliver') {
    return Object.freeze({
      category: 'waiting-approval',
      statusLabel: 'Waiting for your approval',
      reason: 'The current candidate passed its verdict and waits for your approval.',
      nextStep: 'Approve the final Delivery to publish the current candidate.',
      currentRun,
    })
  }
  const verifyingStage = projection.delivery.stages.find(item => (
    item.stage === 'verifying' && (item.status === 'running' || item.status === 'waiting')
  ))
  if (status === 'verifying' || verifyingStage !== undefined) {
    return Object.freeze({
      category: 'verifying',
      statusLabel: 'Verifying',
      reason: 'The current candidate is under verification.',
      nextStep: 'No action is needed. Wait for verification to finish before requesting a verdict.',
      currentRun,
    })
  }
  if (status === 'clarifying') {
    return Object.freeze({
      category: 'waiting-input',
      statusLabel: 'Waiting for your input',
      reason: 'StrongFlow needs answers before planning can start.',
      nextStep: 'Answer the open questions for this Delivery to continue.',
      currentRun,
    })
  }
  if (status === 'draft') {
    return Object.freeze({
      category: 'draft',
      statusLabel: 'Not started',
      reason: null,
      nextStep: 'Start the Delivery to begin its first stage.',
      currentRun,
    })
  }
  const activeRole = currentRun?.role ?? null
  return Object.freeze({
    category: 'active',
    statusLabel: 'In progress',
    reason: activeRole === null ? null : `The ${activeRole} run is working.`,
    nextStep: 'No action is needed right now. StrongFlow continues in the background.',
    currentRun,
  })
}

/** Human label for one realtime connection state. */
export function strongFlowConnectionLabel(realtime: StrongFlowRealtimeStatus): string {
  switch (realtime) {
    case 'subscribed': return 'Live events connected'
    case 'reloading': return 'Refreshing events…'
    case 'reconnecting': return 'Reconnecting…'
    case 'access-revoked': return 'Access revoked'
    case 'closed': return 'Closed'
    default: return 'Not connected'
  }
}

/**
 * Exact execution identity of the canonical active StageRun. Every value is
 * copied from the delivered SessionBinding, StageRun, or Candidate projection;
 * the Delivery snapshot carries no model route, so that row stays unreported
 * instead of being guessed from a thread or worker identifier.
 */
export function strongFlowExecutionIdentity(
  projection: StrongFlowProjection,
  realtime: StrongFlowRealtimeStatus,
): StrongFlowExecutionIdentity {
  const stage = projection.stage
  const binding = stage?.sessionBinding ?? null
  return Object.freeze({
    productSessionId: binding?.productSessionId ?? null,
    stageRunId: typeof stage?.id === 'string' ? stage.id : null,
    attempt: typeof stage?.attempt === 'number' ? stage.attempt : null,
    executionJobId: binding?.executionJobId ?? null,
    workerId: binding?.workerId ?? null,
    workerSessionId: binding?.workerSessionId ?? null,
    codexThreadId: binding?.codexThreadId ?? null,
    modelRoute: null,
    candidateRef: projection.currentCandidate?.candidateRef ?? null,
    leaseId: binding?.leaseId ?? null,
    leaseHeld: (binding?.leaseId ?? null) !== null,
    connection: strongFlowConnectionLabel(realtime),
  })
}

/** Map the identity onto one labeled row per identity kind. */
export function strongFlowIdentityRows(
  identity: StrongFlowExecutionIdentity,
): readonly StrongFlowIdentityRow[] {
  return Object.freeze(IDENTITY_TERMS.map(term => {
    let value: string
    switch (term) {
      case 'ProductSession': value = identity.productSessionId ?? STRONGFLOW_IDENTITY_NOT_REPORTED; break
      case 'StageRun': value = identity.stageRunId ?? STRONGFLOW_IDENTITY_NOT_REPORTED; break
      case 'Attempt':
        value = identity.attempt === null
          ? STRONGFLOW_IDENTITY_NOT_REPORTED
          : String(identity.attempt)
        break
      case 'ExecutionJob': value = identity.executionJobId ?? STRONGFLOW_IDENTITY_NOT_REPORTED; break
      case 'Worker': value = identity.workerId ?? STRONGFLOW_IDENTITY_NOT_REPORTED; break
      case 'WorkerSession': value = identity.workerSessionId ?? STRONGFLOW_IDENTITY_NOT_REPORTED; break
      case 'CodexThread': value = identity.codexThreadId ?? STRONGFLOW_IDENTITY_NOT_REPORTED; break
      case 'Model route': value = identity.modelRoute ?? STRONGFLOW_IDENTITY_NOT_REPORTED; break
      case 'Candidate': value = identity.candidateRef ?? CANDIDATE_NONE; break
      case 'Lease': value = identity.leaseId ?? STRONGFLOW_IDENTITY_NOT_REPORTED; break
      default: value = identity.connection; break
    }
    return Object.freeze({ term, value })
  }))
}

export interface StrongFlowHeaderOptions {
  readonly document: Document
}

export interface StrongFlowHeaderView {
  readonly root: HTMLElement
  update(state: StrongFlowViewModelState): void
  close(): void
}

function setText(node: HTMLElement, text: string): void {
  if (node.textContent !== text) node.textContent = text
}

/**
 * Mount the first-screen header: human status, blocking reason, next step, and
 * a collapsible execution identity card. Updates keep DOM identity by writing
 * text in place, and the whole header hides when no exact snapshot exists.
 */
export function mountStrongFlowHeader(options: StrongFlowHeaderOptions): StrongFlowHeaderView {
  const document = options.document
  const root = strongFlowElement(document, 'section', 'wwc-strongflow-header')
  root.setAttribute('aria-label', 'Current status and next step')
  const status = strongFlowElement(document, 'p', 'wwc-strongflow-header-status')
  status.setAttribute('role', 'status')
  status.setAttribute('aria-live', 'polite')
  const run = strongFlowElement(document, 'p', 'wwc-strongflow-header-run')
  const reason = strongFlowElement(document, 'p', 'wwc-strongflow-header-reason')
  const nextLine = strongFlowElement(document, 'p', 'wwc-strongflow-header-next')
  const identity = strongFlowElement(document, 'div', 'wwc-strongflow-identity')
  const toggle = strongFlowElement(
    document,
    'button',
    'wwc-strongflow-identity-toggle',
  ) as HTMLButtonElement
  toggle.type = 'button'
  const list = strongFlowElement(document, 'dl', 'wwc-strongflow-identity-list')
  list.id = 'wwc-strongflow-identity-list'
  const note = strongFlowElement(document, 'p', 'wwc-strongflow-identity-note')
  note.textContent = 'Values come from the exact Delivery snapshot; absent facts are Not reported.'
  const rows = IDENTITY_TERMS.map(term => {
    const row = document.createElement('div')
    row.className = 'wwc-strongflow-identity-row'
    const termNode = document.createElement('dt')
    const valueNode = document.createElement('dd')
    termNode.textContent = term
    row.append(termNode, valueNode)
    list.append(row)
    return { term, value: valueNode }
  })
  identity.append(toggle, list, note)
  root.append(status, run, reason, nextLine, identity)

  let open = false
  let closed = false
  const onToggle = () => {
    if (closed) return
    open = !open
    applyCollapsed()
  }
  function applyCollapsed(): void {
    toggle.setAttribute('aria-expanded', String(open))
    toggle.setAttribute('aria-controls', list.id)
    list.hidden = !open
    setText(toggle, open ? 'Hide execution identity' : 'Show execution identity')
  }
  toggle.addEventListener('click', onToggle)
  applyCollapsed()

  return {
    root,
    update(state) {
      if (closed) return
      const projection = state.projection
      if (projection === null) {
        root.hidden = true
        return
      }
      root.hidden = false
      const step = strongFlowNextStep(projection)
      root.dataset.category = step.category
      setText(status, step.statusLabel)
      const runText = runSummary(step.currentRun)
      setText(run, runText)
      run.hidden = runText.length === 0
      setText(reason, step.reason ?? '')
      reason.hidden = step.reason === null
      setText(nextLine, `Next step: ${step.nextStep}`)
      const identityRows = strongFlowIdentityRows(
        strongFlowExecutionIdentity(projection, state.realtime),
      )
      identityRows.forEach((row, index) => {
        const valueNode = rows[index]?.value
        if (valueNode !== undefined) setText(valueNode, row.value)
      })
    },
    close() {
      if (closed) return
      closed = true
      toggle.removeEventListener('click', onToggle)
      root.replaceChildren()
      root.remove?.()
    },
  }
}
