// SPDX-License-Identifier: Apache-2.0

import type {
  ControlPlaneCandidateApplyReceipt,
  ControlPlaneCandidateApplyResult,
  ControlPlaneCandidateApplyStrategy,
  ControlPlaneCandidateSummary,
  ControlPlaneDeviceSummary,
  ControlPlaneRunIdentityPort,
  ControlPlaneRunWorkerSessionState,
  ControlPlaneTaskAnchor,
} from './community-control-plane-client.js'
import {
  deviceStateText,
  deviceStateTone,
  type ClientsViewModel,
} from './clients-view-model.js'
import type { RepositoriesViewModel } from './repositories-view-model.js'

/** The one presentation tone of a state or result badge (ADR-0029). */
type CandidateTone = 'info' | 'success' | 'warning' | 'danger' | 'neutral'

/** The displayed candidate states the run page's Candidate row renders. */
type CandidateDisplayState =
  | 'retained'
  | 'branch_created'
  | 'applied'
  | 'conflict'
  | 'discarded'
  | 'failed'

/**
 * Derive the displayed state from the Server projection alone: a live merge
 * conflict rises above the retained states, everything else keeps its honest
 * lifecycle name.
 */
function candidateDisplayState(
  candidate: ControlPlaneCandidateSummary,
): CandidateDisplayState {
  if (candidate.state === 'applied') return 'applied'
  if (candidate.state === 'discarded') return 'discarded'
  if (candidate.state === 'failed') return 'failed'
  for (let index = candidate.history.length - 1; index >= 0; index -= 1) {
    const entry = candidate.history[index]
    if (entry !== undefined && entry.result === 'merge_conflict') return 'conflict'
  }
  return candidate.state
}

/** The one copy per displayed state; every badge also carries the tone. */
function candidateDisplayStateText(state: CandidateDisplayState): string {
  switch (state) {
    case 'retained': return 'Retained on the device'
    case 'branch_created': return 'Local branch created'
    case 'applied': return 'Applied to the target branch'
    case 'conflict': return 'Apply conflict needs attention'
    case 'discarded': return 'Discarded'
    case 'failed': return 'Retention failed'
  }
}

function candidateDisplayStateTone(state: CandidateDisplayState): CandidateTone {
  switch (state) {
    case 'retained': return 'info'
    case 'branch_created': return 'info'
    case 'applied': return 'success'
    case 'conflict': return 'warning'
    case 'discarded': return 'neutral'
    case 'failed': return 'danger'
  }
}

/** The one copy per terminal apply result. */
function candidateResultText(result: ControlPlaneCandidateApplyResult): string {
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

function candidateResultTone(result: ControlPlaneCandidateApplyResult): CandidateTone {
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

/** The short commit form the Apply row shows; the full SHA stays in the title. */
function shortCommitText(commit: string): string {
  return commit.slice(0, 7)
}

/** The Client row of the §16.7 run-page identity zone. */
export interface TaskRunClientFacts {
  readonly displayName: string
  readonly stateText: string
  readonly tone: 'info' | 'success' | 'warning' | 'danger' | 'neutral'
}

/** The Occupancy row of the §16.7 run-page identity zone. */
export interface TaskRunOccupancyFacts {
  readonly stateText: string
  readonly tone: 'info' | 'success' | 'warning' | 'danger' | 'neutral'
  readonly capacityText: string
}

/** The Repository row of the §16.7 run-page identity zone. */
export interface TaskRunRepositoryFacts {
  readonly displayName: string
  readonly defaultBranch: string
}

/** One WorkerSession row of the §16.7 run-page identity zone. */
export interface TaskRunWorkerSessionFacts {
  readonly workerSessionId: string
  readonly state: ControlPlaneRunWorkerSessionState
  readonly stateText: string
  readonly tone: CandidateTone
  readonly startedAt: string | null
}

/** The latest-Candidate row of the §16.7 run-page identity zone. */
export interface TaskRunCandidateFacts {
  readonly candidateRef: string
  readonly stateText: string
  readonly tone: CandidateTone
  readonly branchName: string | null
}

/** The latest-Apply row, derived from the candidate's last ledger receipt. */
export interface TaskRunApplyFacts {
  readonly result: ControlPlaneCandidateApplyResult
  readonly resultText: string
  readonly tone: CandidateTone
  readonly strategy: ControlPlaneCandidateApplyStrategy
  readonly targetBranch: string
  readonly resultingCommit: string | null
  readonly recordedAt: string
}

/** The identity rows the run identity port projects (fake-first, UI-100.2). */
export interface TaskRunIdentityFacts {
  readonly workerSessions: readonly TaskRunWorkerSessionFacts[]
  readonly candidate: TaskRunCandidateFacts | null
  readonly apply: TaskRunApplyFacts | null
}

export type TaskRunZoneStatus = 'loading' | 'ready' | 'unavailable'

/**
 * One §16.7 run-page snapshot.  Client, Occupancy, and Repository facts are
 * projected live from the shell-owned models; the WorkerSession and
 * Candidate/Apply rows come from the run identity port (fake-first until the
 * FLOW routing lands).  A row that has no fact yet stays `null` so the page
 * can name the gap instead of inventing state.
 */
export interface TaskRunState {
  readonly status: 'loading' | 'ready' | 'partial'
  readonly anchor: ControlPlaneTaskAnchor
  readonly taskDescription: string | null
  readonly client: TaskRunClientFacts | null
  readonly occupancy: TaskRunOccupancyFacts | null
  readonly repository: TaskRunRepositoryFacts | null
  readonly identity: TaskRunIdentityFacts | null
  readonly identityStatus: TaskRunZoneStatus
}

export type TaskRunListener = (state: TaskRunState) => void

/** The one copy per WorkerSession state; every badge also carries the tone. */
export function runWorkerSessionStateText(state: ControlPlaneRunWorkerSessionState): string {
  switch (state) {
    case 'reserving': return 'Reserving capacity'
    case 'launching': return 'Launching the worker'
    case 'running': return 'Running'
    case 'draining': return 'Finishing current work'
    case 'stopped': return 'Stopped'
    case 'failed': return 'Failed to start'
  }
}

/** Non-color tone of a WorkerSession state badge (ADR-0029). */
export function runWorkerSessionStateTone(
  state: ControlPlaneRunWorkerSessionState,
): CandidateTone {
  switch (state) {
    case 'reserving': return 'info'
    case 'launching': return 'info'
    case 'running': return 'success'
    case 'draining': return 'warning'
    case 'stopped': return 'neutral'
    case 'failed': return 'danger'
  }
}

function clientFacts(device: ControlPlaneDeviceSummary | undefined): TaskRunClientFacts | null {
  if (device === undefined) return null
  return Object.freeze({
    displayName: device.displayName,
    stateText: deviceStateText(device),
    tone: deviceStateTone(device),
  })
}

function occupancyFacts(
  device: ControlPlaneDeviceSummary | undefined,
): TaskRunOccupancyFacts | null {
  if (device === undefined) return null
  return Object.freeze({
    stateText: deviceStateText(device),
    tone: deviceStateTone(device),
    capacityText: `Capacity ${String(device.capacityUsed)} / ${String(device.capacityTotal)}`,
  })
}

function applyFacts(
  history: readonly ControlPlaneCandidateApplyReceipt[],
): TaskRunApplyFacts | null {
  const latest = history[history.length - 1]
  if (latest === undefined) return null
  return Object.freeze({
    result: latest.result,
    resultText: candidateResultText(latest.result),
    tone: candidateResultTone(latest.result),
    strategy: latest.strategy,
    targetBranch: latest.targetBranch,
    resultingCommit: latest.resultingCommit,
    recordedAt: latest.createdAt,
  })
}

// 设计稿 05:身份折叠行来自 WorkRun 运行投影(WorkRunState → 会话状态映射)。
const WORK_RUN_STATE_TO_SESSION_STATE = Object.freeze({
  queued: 'reserving',
  leased: 'launching',
  running: 'running',
  candidate_ready: 'running',
  settled: 'stopped',
  failed: 'failed',
  cancelled: 'stopped',
})

function identityFacts(projection: {
  readonly workRun: import('./generated/contracts.js').WorkRun
  readonly candidate: import('./generated/contracts.js').Candidate | null
}): TaskRunIdentityFacts {
  const workRun = projection.workRun
  const candidate = projection.candidate
  const state = WORK_RUN_STATE_TO_SESSION_STATE[workRun.state] ?? ('stopped' as ControlPlaneRunWorkerSessionState)
  return Object.freeze({
    workerSessions: Object.freeze([Object.freeze({
      workerSessionId: workRun.workerSessionId,
      state,
      stateText: runWorkerSessionStateText(state),
      tone: runWorkerSessionStateTone(state),
      startedAt: null,
    })]),
    candidate: candidate === null
      ? null
      : Object.freeze({
          candidateRef: candidate.candidateRef,
          stateText: runWorkerSessionStateText(state),
          tone: runWorkerSessionStateTone(state),
          branchName: null,
        }),
    apply: candidate === null ? null : applyFacts([]),
  })
}

/** The short commit form the Apply row shows; the full SHA stays in the title. */
export function taskRunCommitText(commit: string | null): string | null {
  return commit === null ? null : shortCommitText(commit)
}

/**
 * Own the §16.7 run-page projection: the started-task anchor from the route,
 * the live Client/Occupancy/Repository facts from the shell-owned models, and
 * the WorkerSession/Candidate/Apply rows from the run identity port.  The
 * model creates no task state of its own.
 */
export function createTaskRunViewModel(options: {
  readonly anchor: ControlPlaneTaskAnchor
  /** Optional task description re-read from the task port (fake-first). */
  readonly taskDescription?: string | null
  readonly clients: ClientsViewModel
  readonly repositories: RepositoriesViewModel
  readonly identity: ControlPlaneRunIdentityPort
}): TaskRunViewModel {
  const clients = options.clients
  const repositories = options.repositories
  let identity: TaskRunIdentityFacts | null = null
  let identityStatus: TaskRunZoneStatus = 'loading'
  let closed = false
  let identityEpoch = 0

  const listeners = new Set<TaskRunListener>()

  function liveFacts(): {
    readonly client: TaskRunClientFacts | null
    readonly occupancy: TaskRunOccupancyFacts | null
    readonly repository: TaskRunRepositoryFacts | null
  } {
    const device = clients.state.devices.find(
      candidate => candidate.clientId === options.anchor.clientId,
    )
    const repository = repositories.state.clientId === options.anchor.clientId
      ? repositories.state.repositories.find(
          candidate => candidate.repositoryBindingId === options.anchor.repositoryBindingId,
        )
      : undefined
    return {
      client: clientFacts(device),
      occupancy: occupancyFacts(device),
      repository: repository === undefined
        ? null
        : Object.freeze({
            displayName: repository.displayName,
            defaultBranch: repository.defaultBranch,
          }),
    }
  }

  function project(): TaskRunState {
    const live = liveFacts()
    // Every live row and the identity zone report independently; the snapshot
    // is ready only when both sides have served facts, partial when the
    // identity read failed, and loading while the first facts are in flight.
    const status: TaskRunState['status'] = live.client === null && identityStatus === 'loading'
      ? 'loading'
      : identityStatus === 'unavailable'
        ? 'partial'
        : live.client === null || live.repository === null
          ? 'loading'
          : 'ready'
    return Object.freeze({
      status,
      anchor: options.anchor,
      taskDescription: options.taskDescription ?? null,
      client: live.client,
      occupancy: live.occupancy,
      repository: live.repository,
      identity,
      identityStatus,
    })
  }

  let currentState: TaskRunState = project()

  function publish(): void {
    currentState = project()
    for (const listener of listeners) listener(currentState)
  }

  const unsubscribeClients = clients.subscribe(() => {
    if (closed) return
    publish()
  })
  const unsubscribeRepositories = repositories.subscribe(() => {
    if (closed) return
    publish()
  })

  async function readIdentity(): Promise<void> {
    const epoch = ++identityEpoch
    identityStatus = 'loading'
    try {
      const projection = await options.identity.read(options.anchor)
      if (closed || epoch !== identityEpoch) return
      identity = identityFacts(projection)
      identityStatus = 'ready'
    } catch {
      if (closed || epoch !== identityEpoch) return
      // An unavailable identity read keeps the live rows and names the gap;
      // it never invents WorkerSession or Candidate state.
      identity = null
      identityStatus = 'unavailable'
    }
    publish()
  }

  return {
    get state() {
      return currentState
    },
    subscribe(listener) {
      if (closed) return () => {}
      listeners.add(listener)
      listener(currentState)
      return () => { listeners.delete(listener) }
    },
    async start() {
      if (closed) return
      const reads: Array<Promise<unknown>> = [readIdentity()]
      if (clients.state.devicesStatus === 'unloaded') reads.push(clients.refresh())
      if (repositories.state.clientId !== options.anchor.clientId) {
        reads.push(repositories.showDevice(options.anchor.clientId))
      }
      await Promise.allSettled(reads)
      publish()
    },
    async refresh() {
      if (closed) return
      await Promise.allSettled([
        clients.refresh(),
        repositories.refresh(),
        readIdentity(),
      ])
      publish()
    },
    close() {
      if (closed) return
      closed = true
      identityEpoch += 1
      unsubscribeClients()
      unsubscribeRepositories()
      listeners.clear()
    },
  }
}

export interface TaskRunViewModel {
  /** The current run snapshot; every listener receives it on subscribe. */
  readonly state: TaskRunState
  subscribe(listener: TaskRunListener): () => void
  /** First read: live facts plus the identity zone. */
  start(): Promise<void>
  /** Re-read every source; a failed read keeps the shown rows. */
  refresh(): Promise<void>
  close(): void
}
