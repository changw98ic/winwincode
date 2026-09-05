// SPDX-License-Identifier: Apache-2.0

import {
  ControlPlaneClientError,
  controlPlaneCandidateActionFailure,
  type ControlPlaneCandidateActionFailure,
  type ControlPlaneCandidateApplyResult,
  type ControlPlaneCandidateApplyReceipt,
  type ControlPlaneCandidateBranchOutcome,
  type ControlPlaneCandidateState,
  type ControlPlaneCandidateSummary,
} from './control-plane-client.js'

/**
 * The candidate seam the card controls consume, derived from the frozen
 * candidate facade (GIT-100.8). Every method resolves when the Server accepted
 * the request and rejects with the one `ControlPlaneClientError` identity; the
 * resulting lifecycle fact always arrives through the next candidate-list
 * snapshot, never from this return value.
 */
export interface LocalCandidatePort {
  /** Read the candidate cards projected for one Client device. */
  listCandidates(input: {
    readonly clientId: string
  }): Promise<readonly ControlPlaneCandidateSummary[]>
  /** Create (or reuse) the local branch of one candidate; repeats are safe. */
  createBranch(input: {
    readonly clientId: string
    readonly candidateRef: string
    readonly repositoryBindingId: string
  }): Promise<ControlPlaneCandidateBranchOutcome>
  /** Apply one candidate onto a target branch under an expected HEAD. */
  apply(input: {
    readonly clientId: string
    readonly candidateRef: string
    readonly repositoryBindingId: string
    readonly targetBranch: string
    readonly expectedHead: string
  }): Promise<ControlPlaneCandidateApplyReceipt>
  /** Discard one retained candidate. */
  discard(input: {
    readonly clientId: string
    readonly candidateRef: string
    readonly repositoryBindingId: string
  }): Promise<ControlPlaneCandidateSummary>
}

/** The three card actions a candidate offers. */
export type LocalCandidateAction = 'branch' | 'apply' | 'discard'

/** The two dangerous actions that must pass an explicit confirmation. */
export type LocalCandidateDangerAction = 'apply' | 'discard'

/**
 * The one candidate interaction a card can be in. The apply and discard
 * confirmations are armed here; the typed apply inputs live in the mounted
 * card until the explicit accept submits them.
 */
export type LocalCandidateInteraction =
  | { readonly kind: 'rest' }
  | { readonly kind: 'confirming-apply' }
  | { readonly kind: 'confirming-discard' }
  | { readonly kind: 'submitting'; readonly action: LocalCandidateAction }
  | {
    readonly kind: 'failed'
    readonly action: LocalCandidateAction
    readonly failure: ControlPlaneCandidateActionFailure
  }

/** The five card states the task names, plus the honest retention failure. */
export type LocalCandidateDisplayState =
  | 'retained'
  | 'branch_created'
  | 'applied'
  | 'conflict'
  | 'discarded'
  | 'failed'

/** The one presentation tone of a state or result badge (ADR-0029). */
export type LocalCandidateTone = 'info' | 'success' | 'warning' | 'danger' | 'neutral'

/** The whole candidate area snapshot the page renders from. */
export interface LocalCandidateViewModelState {
  readonly status: 'unloaded' | 'loading' | 'ready' | 'unavailable'
  readonly clientId: string | null
  readonly candidates: readonly ControlPlaneCandidateSummary[]
}

export type LocalCandidateViewModelListener = (state: LocalCandidateViewModelState) => void

/**
 * Whether the Server projection still allows the local branch creation: only
 * a plainly retained candidate, before its branch exists.
 */
export function candidateSupportsBranch(candidate: ControlPlaneCandidateSummary): boolean {
  return candidate.state === 'retained'
}

/** Whether the card may offer the dangerous apply entry right now. */
export function candidateSupportsApply(candidate: ControlPlaneCandidateSummary): boolean {
  return candidate.state === 'retained' || candidate.state === 'branch_created'
}

/** Whether the card may offer the dangerous discard entry right now. */
export function candidateSupportsDiscard(candidate: ControlPlaneCandidateSummary): boolean {
  return candidate.state === 'retained'
    || candidate.state === 'branch_created'
    || candidate.state === 'failed'
}

/**
 * Derive the displayed card state from the Server projection alone: a live
 * merge conflict rises above the retained states, everything else keeps its
 * honest lifecycle name.
 */
export function candidateDisplayState(candidate: ControlPlaneCandidateSummary): LocalCandidateDisplayState {
  if (candidate.state === 'applied') return 'applied'
  if (candidate.state === 'discarded') return 'discarded'
  if (candidate.state === 'failed') return 'failed'
  for (let index = candidate.history.length - 1; index >= 0; index -= 1) {
    const entry = candidate.history[index]
    if (entry !== undefined && entry.result === 'merge_conflict') return 'conflict'
  }
  return candidate.state
}

/** The one copy per displayed card state; every badge also carries the tone. */
export function candidateDisplayStateText(state: LocalCandidateDisplayState): string {
  switch (state) {
    case 'retained': return 'Retained on the device'
    case 'branch_created': return 'Local branch created'
    case 'applied': return 'Applied to the target branch'
    case 'conflict': return 'Apply conflict needs attention'
    case 'discarded': return 'Discarded'
    case 'failed': return 'Retention failed'
  }
}

export function candidateDisplayStateTone(state: LocalCandidateDisplayState): LocalCandidateTone {
  switch (state) {
    case 'retained': return 'info'
    case 'branch_created': return 'info'
    case 'applied': return 'success'
    case 'conflict': return 'warning'
    case 'discarded': return 'neutral'
    case 'failed': return 'danger'
  }
}

/**
 * The one copy per terminal apply result. Every entry of the ten-result-code
 * ledger reaches the screen reader through the result history rows.
 */
export function candidateResultText(result: ControlPlaneCandidateApplyResult): string {
  switch (result) {
    case 'retained': return 'Still retained locally.'
    case 'branch_created': return 'Local branch created.'
    case 'applied': return 'Applied to the target branch.'
    case 'base_stale': return 'The target branch moved ahead. Refresh the expected HEAD and retry.'
    case 'working_tree_dirty': return 'The target worktree has uncommitted changes. Settle them first.'
    case 'merge_conflict': return 'Conflicts must be resolved before this apply can land.'
    case 'candidate_missing': return 'The candidate ref is gone from the device.'
    case 'permission_denied': return 'You lack permission for the target repository.'
    case 'discarded': return 'The candidate was discarded.'
    case 'failed': return 'The apply failed. Check the device and try again.'
  }
}

export function candidateResultTone(result: ControlPlaneCandidateApplyResult): LocalCandidateTone {
  switch (result) {
    case 'retained': return 'info'
    case 'branch_created': return 'info'
    case 'applied': return 'success'
    case 'base_stale': return 'warning'
    case 'working_tree_dirty': return 'warning'
    case 'merge_conflict': return 'warning'
    case 'candidate_missing': return 'danger'
    case 'permission_denied': return 'danger'
    case 'discarded': return 'neutral'
    case 'failed': return 'danger'
  }
}

/** The short commit form the cards show; the full SHA stays in the title. */
export function shortCommitText(commit: string): string {
  return commit.slice(0, 7)
}

/**
 * Build the downloadable summary of one candidate's latest conflict from the
 * receipt fields only. Repository worktree paths, credentials, and diff
 * content never enter the summary, so the text is safe to share.
 */
export function localCandidateConflictSummaryText(
  candidate: ControlPlaneCandidateSummary,
): string | null {
  let conflict: ControlPlaneCandidateApplyReceipt | null = null
  for (let index = candidate.history.length - 1; index >= 0; index -= 1) {
    const entry = candidate.history[index]
    if (entry !== undefined && entry.result === 'merge_conflict') {
      conflict = entry
      break
    }
  }
  if (conflict === null) return null
  return [
    'Candidate apply conflict summary',
    `Candidate ref: ${candidate.candidateRef}`,
    `Candidate commit: ${shortCommitText(candidate.candidateCommit)} (${candidate.candidateCommit})`,
    `Repository binding: ${candidate.repositoryBindingId}`,
    `Target branch: ${conflict.targetBranch}`,
    `Expected HEAD: ${conflict.expectedHead}`,
    `Strategy: ${conflict.strategy}`,
    `Result: ${conflict.result}`,
    `Resulting commit: ${conflict.resultingCommit ?? 'none'}`,
    `Conflict artifact: ${conflict.conflictArtifactRef ?? 'none'}`,
    `Recorded at: ${conflict.createdAt}`,
    'Resolve the conflicts on the device, then retry the apply with a fresh expected HEAD.',
  ].join('\n')
}

/**
 * The one occupancy-style wire-code translation used by the default
 * classifier; tests inject deterministic classifiers through `classify`.
 */
export function localCandidateActionFailure(error: unknown): ControlPlaneCandidateActionFailure {
  return controlPlaneCandidateActionFailure(error)
}

const UNLOADED: LocalCandidateViewModelState = Object.freeze({
  status: 'unloaded',
  clientId: null,
  candidates: Object.freeze([]),
})

const REST: LocalCandidateInteraction = Object.freeze({ kind: 'rest' })

/**
 * Own the per-candidate interaction state machine for one Client device's
 * candidate area. The candidate-list snapshot stays the single lifecycle
 * authority: this model only arms confirmations, submits one deduplicated
 * request per candidate, and re-reads the projection after a request lands.
 */
export interface LocalCandidateViewModel {
  /** The current area snapshot; every listener receives it on subscribe. */
  readonly state: LocalCandidateViewModelState
  /** The current interaction of one candidate; unknown candidates rest. */
  interaction(candidateRef: string): LocalCandidateInteraction
  /** Re-read the candidate projection of one Client device. */
  refresh(clientId: string): Promise<void>
  /** Create the local branch; a busy candidate is never repeated. */
  requestBranch(candidateRef: string): void
  /** Arm the explicit apply confirmation for one candidate. */
  requestApply(candidateRef: string): void
  /**
   * Submit the armed apply with the exact target branch and expected HEAD;
   * only a confirmation or a failed draft submits, and empty inputs never do.
   */
  confirmApply(
    candidateRef: string,
    input: { readonly targetBranch: string; readonly expectedHead: string },
  ): void
  /** Arm the explicit discard confirmation for one candidate. */
  requestDiscard(candidateRef: string): void
  /** Submit the armed discard; only a confirmation or a failed draft submits. */
  confirmDiscard(candidateRef: string): void
  /** Drop the armed confirmation or the shown failure of one candidate. */
  dismiss(candidateRef: string): void
  /**
   * The safe conflict summary text of one candidate's latest conflicted
   * attempt, or null while the projection carries no conflict receipt.
   */
  conflictSummary(candidateRef: string): string | null
  subscribe(listener: LocalCandidateViewModelListener): () => void
  close(): void
}

function interactionKey(interaction: LocalCandidateInteraction): string {
  if (interaction.kind === 'rest') return 'rest'
  if (interaction.kind === 'submitting') return `submitting:${interaction.action}`
  if (interaction.kind === 'failed') {
    return `failed:${interaction.action}:${interaction.failure}`
  }
  return interaction.kind
}

export function createLocalCandidateViewModel(options: {
  readonly port: LocalCandidatePort | null
  /** Facade-owned classifier seam; the default reads stable wire codes only. */
  readonly classify?: (error: unknown) => ControlPlaneCandidateActionFailure
}): LocalCandidateViewModel {
  const classify = options.classify ?? localCandidateActionFailure
  let state = UNLOADED
  const interactions = new Map<string, LocalCandidateInteraction>()
  const listeners = new Set<LocalCandidateViewModelListener>()
  let closed = false
  let refreshEpoch = 0

  function publish(): void {
    for (const listener of listeners) listener(state)
  }

  function setState(next: LocalCandidateViewModelState): void {
    state = next
    publish()
  }

  function findCandidate(candidateRef: string): ControlPlaneCandidateSummary | undefined {
    return state.candidates.find(entry => entry.candidateRef === candidateRef)
  }

  function setInteraction(
    candidateRef: string,
    interaction: LocalCandidateInteraction,
  ): void {
    const previous = interactions.get(candidateRef) ?? REST
    if (interactionKey(previous) === interactionKey(interaction)) return
    interactions.set(candidateRef, interaction)
    publish()
  }

  /**
   * A snapshot that moved past the armed intent or the shown failure drops
   * it: the world changed, so the draft is stale and the card returns to the
   * actions the new projection supports.
   */
  function pruneStaleInteractions(): void {
    let changed = false
    for (const [candidateRef, interaction] of interactions) {
      if (interaction.kind === 'rest' || interaction.kind === 'submitting') continue
      const candidate = findCandidate(candidateRef)
      const action: LocalCandidateAction = interaction.kind === 'confirming-discard'
        ? 'discard'
        : interaction.kind === 'failed'
          ? interaction.action
          : 'apply'
      const stale = candidate === undefined
        || (action === 'apply' && !candidateSupportsApply(candidate))
        || (action === 'discard' && !candidateSupportsDiscard(candidate))
      if (!stale) continue
      interactions.set(candidateRef, REST)
      changed = true
    }
    if (changed) publish()
  }

  function actionApplies(
    action: LocalCandidateAction,
    candidate: ControlPlaneCandidateSummary,
  ): boolean {
    if (action === 'branch') return candidateSupportsBranch(candidate)
    if (action === 'apply') return candidateSupportsApply(candidate)
    return candidateSupportsDiscard(candidate)
  }

  async function refresh(clientId: string): Promise<void> {
    const port = options.port
    if (port === null) {
      setState({ status: 'unavailable', clientId, candidates: state.candidates })
      return
    }
    const epoch = ++refreshEpoch
    setState({ status: 'loading', clientId, candidates: state.candidates })
    try {
      const candidates = await port.listCandidates({ clientId })
      if (closed || epoch !== refreshEpoch) return
      setState({ status: 'ready', clientId, candidates })
      pruneStaleInteractions()
    } catch {
      if (closed || epoch !== refreshEpoch) return
      // A failed read never discards the shown cards (ADR-0029 snapshot rule).
      setState({ status: 'unavailable', clientId, candidates: state.candidates })
    }
  }

  async function submit(
    candidateRef: string,
    action: LocalCandidateAction,
    run: () => Promise<unknown>,
  ): Promise<void> {
    setInteraction(candidateRef, { kind: 'submitting', action })
    try {
      await run()
    } catch (error) {
      if (closed) return
      setInteraction(candidateRef, { kind: 'failed', action, failure: classify(error) })
      return
    }
    if (closed) return
    setInteraction(candidateRef, REST)
    // The Server snapshot stays the single lifecycle authority, so every
    // landed action re-reads the candidate list instead of migrating the card.
    const clientId = state.clientId
    if (clientId !== null) await refresh(clientId)
  }

  /**
   * One guard for every entry: an in-flight request is never repeated, and a
   * request the current projection no longer supports is ignored. Returns the
   * validating candidate fact so the caller can decide the confirmation path.
   */
  function requestCandidate(
    candidateRef: string,
    action: LocalCandidateAction,
  ): ControlPlaneCandidateSummary | null {
    if (closed) return null
    const current = interactions.get(candidateRef) ?? REST
    if (current.kind === 'submitting') return null
    const candidate = findCandidate(candidateRef)
    if (candidate === undefined || !actionApplies(action, candidate)) return null
    return candidate
  }

  return {
    get state() {
      return state
    },
    interaction(candidateRef) {
      return interactions.get(candidateRef) ?? REST
    },
    refresh(clientId) {
      if (closed) return Promise.resolve()
      return refresh(clientId)
    },
    requestBranch(candidateRef) {
      const candidate = requestCandidate(candidateRef, 'branch')
      if (candidate === null) return
      // Branch creation never touches the user's worktree, so it submits
      // without a confirmation and repeats return the original branch.
      void submit(candidateRef, 'branch', () => {
        const port = options.port
        if (port === null) return Promise.reject(new ControlPlaneClientError({
          kind: 'protocol',
          code: 'CANDIDATE_PORT_UNAVAILABLE',
          message: 'The candidate port is unavailable.',
          requestId: null,
          retryable: false,
        }))
        return port.createBranch({
          clientId: state.clientId ?? '',
          candidateRef,
          repositoryBindingId: candidate.repositoryBindingId,
        })
      })
    },
    requestApply(candidateRef) {
      if (requestCandidate(candidateRef, 'apply') === null) return
      setInteraction(candidateRef, { kind: 'confirming-apply' })
    },
    confirmApply(candidateRef, input) {
      if (closed) return
      const current = interactions.get(candidateRef) ?? REST
      const armed = current.kind === 'confirming-apply'
        || (current.kind === 'failed' && current.action === 'apply')
      if (!armed) return
      const targetBranch = input.targetBranch.trim()
      const expectedHead = input.expectedHead.trim()
      if (targetBranch.length === 0 || expectedHead.length === 0) return
      const candidate = findCandidate(candidateRef)
      if (candidate === undefined) return
      void submit(candidateRef, 'apply', () => {
        const port = options.port
        if (port === null) return Promise.reject(new ControlPlaneClientError({
          kind: 'protocol',
          code: 'CANDIDATE_PORT_UNAVAILABLE',
          message: 'The candidate port is unavailable.',
          requestId: null,
          retryable: false,
        }))
        return port.apply({
          clientId: state.clientId ?? '',
          candidateRef,
          repositoryBindingId: candidate.repositoryBindingId,
          targetBranch,
          expectedHead,
        })
      })
    },
    requestDiscard(candidateRef) {
      if (requestCandidate(candidateRef, 'discard') === null) return
      setInteraction(candidateRef, { kind: 'confirming-discard' })
    },
    confirmDiscard(candidateRef) {
      if (closed) return
      const current = interactions.get(candidateRef) ?? REST
      const armed = current.kind === 'confirming-discard'
        || (current.kind === 'failed' && current.action === 'discard')
      if (!armed) return
      const candidate = findCandidate(candidateRef)
      if (candidate === undefined) return
      void submit(candidateRef, 'discard', () => {
        const port = options.port
        if (port === null) return Promise.reject(new ControlPlaneClientError({
          kind: 'protocol',
          code: 'CANDIDATE_PORT_UNAVAILABLE',
          message: 'The candidate port is unavailable.',
          requestId: null,
          retryable: false,
        }))
        return port.discard({
          clientId: state.clientId ?? '',
          candidateRef,
          repositoryBindingId: candidate.repositoryBindingId,
        })
      })
    },
    dismiss(candidateRef) {
      if (closed) return
      const current = interactions.get(candidateRef) ?? REST
      if (current.kind !== 'confirming-apply' && current.kind !== 'confirming-discard'
        && current.kind !== 'failed') return
      setInteraction(candidateRef, REST)
    },
    conflictSummary(candidateRef) {
      const candidate = findCandidate(candidateRef)
      if (candidate === undefined) return null
      return localCandidateConflictSummaryText(candidate)
    },
    subscribe(listener) {
      if (closed) return () => {}
      listeners.add(listener)
      listener(state)
      return () => {
        listeners.delete(listener)
      }
    },
    close() {
      if (closed) return
      closed = true
      refreshEpoch += 1
      listeners.clear()
      interactions.clear()
      setState({ status: 'unloaded', clientId: null, candidates: Object.freeze([]) })
    },
  }
}

type CandidateFacadeMethod = (input: Record<string, unknown>) => Promise<unknown>

function facadeMethod(value: unknown): CandidateFacadeMethod | null {
  if (typeof value !== 'function') return null
  return value as CandidateFacadeMethod
}

/**
 * Adapt the frozen candidate facade to the port the card controls consume.
 * Returns null while the facade methods have not landed, so hosts compose the
 * port only when the seam exists.
 */
export function localCandidatePortFromFacade(facade: object): LocalCandidatePort | null {
  const methods = facade as Record<string, unknown>
  const listCandidates = facadeMethod(methods['listDeviceCandidates'])
  const createBranch = facadeMethod(methods['createCandidateBranch'])
  const apply = facadeMethod(methods['applyCandidate'])
  const discard = facadeMethod(methods['discardCandidate'])
  if (
    listCandidates === null
    || createBranch === null
    || apply === null
    || discard === null
  ) {
    return null
  }
  return {
    listCandidates: input => listCandidates(input) as Promise<readonly ControlPlaneCandidateSummary[]>,
    createBranch: input => createBranch(input) as Promise<ControlPlaneCandidateBranchOutcome>,
    apply: input => apply(input) as Promise<ControlPlaneCandidateApplyReceipt>,
    discard: input => discard(input) as Promise<ControlPlaneCandidateSummary>,
  }
}
