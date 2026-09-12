// SPDX-License-Identifier: Apache-2.0

import type {
  ControlPlaneCandidateApplyReceipt,
  ControlPlaneCandidateApplyResult,
  ControlPlaneCandidateApplyStrategy,
  ControlPlaneDeviceSummary,
  ControlPlaneRunIdentityPort,
  ControlPlaneRunIdentityProjection,
  ControlPlaneRunCandidateProjection,
  ControlPlaneRunWorkerSessionState,
  ControlPlaneTaskAnchor,
} from './community-control-plane-client.js'
import type {
  DeliveryEvidenceProjection,
  WorkContract,
  WorkGraphItemState,
} from './generated/contracts.js'
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
  | 'produced'
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
  candidate: ControlPlaneRunCandidateProjection,
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
    case 'produced': return '候选结果已生成'
    case 'retained': return '已保留在设备上'
    case 'branch_created': return '已创建本地分支'
    case 'applied': return '已应用到目标分支'
    case 'conflict': return '应用冲突，需要处理'
    case 'discarded': return '已丢弃'
    case 'failed': return '保留失败'
  }
}

function candidateDisplayStateTone(state: CandidateDisplayState): CandidateTone {
  switch (state) {
    case 'produced': return 'info'
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
    case 'retained': return '仍保留在本地。'
    case 'branch_created': return '已创建本地分支。'
    case 'applied': return '已应用到目标分支。'
    case 'base_stale': return '目标分支已有新提交，请刷新预期 HEAD 后重试。'
    case 'working_tree_dirty': return '目标工作区有未提交改动，请先处理。'
    case 'merge_conflict': return '必须先解决冲突才能应用。'
    case 'candidate_missing': return '设备上的候选引用已不存在。'
    case 'permission_denied': return '你没有目标仓库的权限。'
    case 'discarded': return '候选结果已丢弃。'
    case 'failed': return '应用失败，请检查设备后重试。'
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

/** The WorkContract row bound to this WorkItem. */
export interface TaskRunContractFacts {
  readonly contractId: string
  readonly objective: string
  readonly revision: number
  readonly authorityText: string
}

/** One acceptance criterion selected by this WorkItem. */
export interface TaskRunCriterionFacts {
  readonly id: string
  readonly description: string
  readonly required: boolean
  readonly verificationMethod: string | null
}

/** One Evidence record bound to this exact WorkRun. */
export interface TaskRunEvidenceItemFacts {
  readonly evidenceId: string
  readonly typeText: string
  readonly sourceRef: string
}

/** The identity rows projected from the canonical WorkRun and Delivery reads. */
export interface TaskRunIdentityFacts {
  readonly workerSessions: readonly TaskRunWorkerSessionFacts[]
  readonly contract: TaskRunContractFacts | null
  readonly criteria: readonly TaskRunCriterionFacts[]
  readonly owner: string | null
  readonly dependencies: readonly string[]
  readonly blockers: readonly string[]
  readonly graphState: WorkGraphItemState | null
  readonly graphStateText: string | null
  readonly evidence: readonly TaskRunEvidenceItemFacts[]
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
    case 'reserving': return '正在预留容量'
    case 'launching': return '正在启动执行进程'
    case 'running': return '运行中'
    case 'draining': return '正在完成当前工作'
    case 'stopped': return '已停止'
    case 'failed': return '启动失败'
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

const WORK_GRAPH_STATE_TEXT: Readonly<Record<WorkGraphItemState, string>> = Object.freeze({
  ready: '可执行',
  running: '运行中',
  blocked: '已阻塞',
  done: '已完成',
})

const WORK_GRAPH_STATE_TONE: Readonly<Record<WorkGraphItemState, CandidateTone>> = Object.freeze({
  ready: 'info',
  running: 'success',
  blocked: 'warning',
  done: 'neutral',
})

export function runWorkGraphStateTone(state: WorkGraphItemState): CandidateTone {
  return WORK_GRAPH_STATE_TONE[state]
}

function contractAuthorityText(authority: WorkContract['requiredHumanAuthority']): string {
  switch (authority) {
    case 'none': return '无需人工授权'
    case 'approval': return '需要人工批准'
    case 'attention': return '需要人工关注'
  }
}

function evidenceTypeText(type: DeliveryEvidenceProjection['type']): string {
  switch (type) {
    case 'test': return '测试'
    case 'command': return '命令'
    case 'diff': return '差异'
    case 'file': return '文件'
    case 'commit': return '提交'
    case 'pull_request': return '拉取请求'
    case 'runtime_event': return '运行事件'
    case 'review_finding': return '审核发现'
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
    capacityText: `容量 ${String(device.capacityUsed)} / ${String(device.capacityTotal)}`,
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

function identityFacts(projection: ControlPlaneRunIdentityProjection): TaskRunIdentityFacts {
  const workRun = projection.workRun
  const candidate = projection.candidate
  const state = WORK_RUN_STATE_TO_SESSION_STATE[workRun.state] ?? ('stopped' as ControlPlaneRunWorkerSessionState)
  const criteria = projection.contract !== null && projection.item !== null
    ? projection.contract.criteria.filter(criterion => projection.item?.criterionIds.includes(criterion.id))
    : []
  return Object.freeze({
    workerSessions: Object.freeze([Object.freeze({
      workerSessionId: workRun.workerSessionId,
      state,
      stateText: runWorkerSessionStateText(state),
      tone: runWorkerSessionStateTone(state),
      startedAt: null,
    })]),
    contract: projection.contract === null
      ? null
      : Object.freeze({
          contractId: projection.contract.id,
          objective: projection.contract.objective,
          revision: projection.contract.revision,
          authorityText: contractAuthorityText(projection.contract.requiredHumanAuthority),
        }),
    criteria: Object.freeze(criteria.map(criterion => Object.freeze({
      id: criterion.id,
      description: criterion.description,
      required: criterion.required,
      verificationMethod: criterion.verificationMethod,
    }))),
    owner: projection.owner,
    dependencies: projection.graphItem?.dependencies ?? projection.item?.dependsOn ?? Object.freeze([]),
    blockers: projection.graphItem?.blockers ?? Object.freeze([]),
    graphState: projection.graphItem?.state ?? null,
    graphStateText: projection.graphItem === null
      ? null
      : WORK_GRAPH_STATE_TEXT[projection.graphItem.state],
    evidence: Object.freeze(projection.evidence.map(entry => Object.freeze({
      evidenceId: entry.id,
      typeText: evidenceTypeText(entry.type),
      sourceRef: entry.sourceRef,
    }))),
    candidate: candidate === null
      ? null
      : Object.freeze({
          candidateRef: candidate.candidateRef,
          stateText: candidateDisplayStateText(candidateDisplayState(candidate)),
          tone: candidateDisplayStateTone(candidateDisplayState(candidate)),
          branchName: candidate.branchName,
        }),
    apply: candidate === null ? null : applyFacts(candidate.history),
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
  let runTarget: { readonly clientId: string; readonly repositoryBindingId: string } | null = null
  let identityStatus: TaskRunZoneStatus = 'loading'
  let closed = false
  let identityEpoch = 0

  const listeners = new Set<TaskRunListener>()

  function liveFacts(): {
    readonly client: TaskRunClientFacts | null
    readonly occupancy: TaskRunOccupancyFacts | null
    readonly repository: TaskRunRepositoryFacts | null
  } {
    const target = runTarget ?? options.anchor
    const device = clients.state.devices.find(
      candidate => candidate.clientId === target.clientId,
    )
    const repository = repositories.state.clientId === target.clientId
      ? repositories.state.repositories.find(
          candidate => candidate.repositoryBindingId === target.repositoryBindingId,
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
      runTarget = Object.freeze({
        clientId: projection.clientId,
        repositoryBindingId: projection.repositoryBindingId,
      })
      if (repositories.state.clientId !== projection.clientId) {
        await repositories.showDevice(projection.clientId)
      }
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
