// SPDX-License-Identifier: Apache-2.0

import type {
  DeliveryDetailProjection,
  WorkRunId,
  WorkRunAggregateProjection,
  WorkItemId,
  WorkItem,
} from './generated/contracts.js'
import type { StrongFlowProjection } from './strongflow-view-model.js'
import type { StrongFlowHistorySelection } from './strongflow-history-selection.js'
import {
  boundedItems,
  type BoundedItems,
  type StrongFlowRenderLimits,
} from './strongflow-rendering.js'

/** One already-delivered Evidence row of a historical WorkRun. */
export type StrongFlowHistoryEvidence = DeliveryDetailProjection['evidence'][number]

export interface StrongFlowHistoryBinding {
  readonly productSessionId: string | null
  readonly executionJobId: string
  readonly workerId: string | null
  readonly workerSessionId: string | null
  readonly codexThreadId: string | null
}

export interface StrongFlowHistoryRun {
  readonly workRunId: WorkRunId
  readonly workItemId: WorkItemId
  readonly stage: string | null
  readonly role: string | null
  readonly actorType: 'codex' | 'human' | null
  readonly attempt: number | null
  readonly status: string
  readonly startedAt: string | null
  readonly finishedAt: string | null
  readonly isCurrent: boolean
  readonly producedCurrentCandidate: boolean
  readonly evidenceCount: number
  readonly evidence: readonly StrongFlowHistoryEvidence[]
  readonly candidateRefs: readonly string[]
  readonly binding: StrongFlowHistoryBinding | null
}

export interface StrongFlowHistoryTaskNode {
  readonly task: WorkItem
  readonly runs: readonly StrongFlowHistoryRun[]
}

export interface StrongFlowHistoryTree {
  readonly readCursor: StrongFlowProjection['delivery']['readCursor']
  readonly tasks: readonly StrongFlowHistoryTaskNode[]
  readonly deliveryRuns: readonly StrongFlowHistoryRun[]
  readonly runs: readonly StrongFlowHistoryRun[]
  readonly currentWorkRunId: WorkRunId | null
  readonly currentCandidateRef: string | null
  readonly omittedTasks: number
  readonly omittedRuns: number
}

/** Queued or active executable WorkRuns eligible for server cancellation. */
export function strongFlowCancellableWorkRuns(
  projection: StrongFlowProjection,
): readonly WorkRunAggregateProjection['runs'][number][] {
  const aggregate = projection.workRunAggregate
  if (aggregate === undefined) return Object.freeze([])
  return Object.freeze(aggregate.runs.filter(run => (
    run.state === 'queued' || run.state === 'leased' || run.state === 'running'
  )))
}

/**
 * Selects exactly one cancellation target. A caller must provide an explicit
 * WorkRunId when more than one active run exists; this never falls back to a
 * Chat ProductSession or a historical StageRun id.
 */
export function strongFlowCancellationTarget(
  projection: StrongFlowProjection,
  requestedWorkRunId: WorkRunId | null,
): WorkRunAggregateProjection['runs'][number] | null {
  const active = strongFlowCancellableWorkRuns(projection)
  if (requestedWorkRunId !== null) {
    return active.find(run => run.id === requestedWorkRunId) ?? null
  }
  return active.length === 1 ? active[0] ?? null : null
}

function aggregateHistoryRun(
  run: WorkRunAggregateProjection['runs'][number],
  evidence: StrongFlowProjection['evidence'],
  currentWorkRunId: WorkRunId | null,
  currentCandidateRef: string | null,
): StrongFlowHistoryRun {
  const runEvidence = evidence.filter(item => item.workRunId === run.id)
  const candidateRefs = [...new Set(runEvidence.map(item => item.candidateRef))]
  return Object.freeze({
    workRunId: run.id,
    workItemId: run.workItemId,
    stage: null,
    role: null,
    actorType: 'codex',
    attempt: run.attempt,
    status: run.state,
    startedAt: null,
    finishedAt: null,
    isCurrent: run.id === currentWorkRunId,
    producedCurrentCandidate: currentCandidateRef !== null && candidateRefs.includes(currentCandidateRef),
    evidenceCount: runEvidence.length,
    evidence: Object.freeze([...runEvidence]),
    candidateRefs: Object.freeze(candidateRefs),
    // WorkRun is the execution authority. StageRun bindings can enrich the
    // display, but must never replace its job, worker, or session identities.
    binding: Object.freeze({
      productSessionId: run.productSessionId,
      executionJobId: run.executionJobId,
      workerId: run.workerId,
      workerSessionId: run.workerSessionId,
      codexThreadId: run.codexThreadId ?? null,
    }),
  })
}

function boundedItemsWithPinnedIdentity<Value>(
  values: readonly Value[],
  limit: number,
  isPinned: (value: Value) => boolean,
): BoundedItems<Value> {
  const bounded = boundedItems(values, limit)
  if (bounded.items.some(isPinned)) return bounded
  const pinned = values.find(isPinned)
  if (pinned === undefined) return bounded
  return Object.freeze({
    items: Object.freeze([...bounded.items.slice(0, -1), pinned]),
    omitted: bounded.omitted,
  })
}

/**
 * Project one bounded Delivery snapshot onto the navigable history tree.
 * The association truth is each WorkRun's `workItemId`. Derive it once per
 * snapshot and share it across every history view; legacy stage rows are not
 * consulted for execution or identity facts.
 */
export function strongFlowHistoryTree(
  projection: StrongFlowProjection,
  limits: StrongFlowRenderLimits,
  pinnedSelection?: StrongFlowHistorySelection,
): StrongFlowHistoryTree {
  // A projection without the WorkRun read is a bounded loading state; fail
  // closed with an empty history rather than deriving runs from StageRuns.
  const aggregate = projection.workRunAggregate
  if (aggregate === undefined) return Object.freeze({
    readCursor: projection.delivery.readCursor,
    tasks: Object.freeze([]),
    deliveryRuns: Object.freeze([]),
    runs: Object.freeze([]),
    currentWorkRunId: null,
    currentCandidateRef: projection.currentCandidate?.candidateRef ?? null,
    omittedTasks: 0,
    omittedRuns: 0,
  })
  const selectedWorkRunId = projection.runtime.workRunId !== null
    && aggregate.runs.some(run => run.id === projection.runtime.workRunId)
    ? projection.runtime.workRunId
    : null
  const activeRuns = aggregate.runs.filter(run => (
    run.state === 'queued'
    || run.state === 'leased'
    || run.state === 'running'
    || run.state === 'candidate_ready'
  ))
  const currentWorkRunId = selectedWorkRunId
    ?? (activeRuns.length === 1 ? activeRuns[0]?.id ?? null : null)
  const currentCandidateRef = projection.currentCandidate?.candidateRef ?? null
  const taskIds = new Set<string>(aggregate.items.map(task => task.id))
  const runs = boundedItemsWithPinnedIdentity(
    aggregate.runs,
    limits.stages,
    run => run.id === pinnedSelection?.workRunId,
  ).items.map(run => aggregateHistoryRun(
    run,
    projection.evidence,
    currentWorkRunId,
    currentCandidateRef,
  ))
  const runsByTask = new Map<string, StrongFlowHistoryRun[]>()
  const deliveryRuns: StrongFlowHistoryRun[] = []
  for (const run of runs) {
    if (!taskIds.has(run.workItemId)) {
      deliveryRuns.push(run)
      continue
    }
    const owned = runsByTask.get(run.workItemId) ?? []
    owned.push(run)
    runsByTask.set(run.workItemId, owned)
  }
  const boundedTasks = boundedItemsWithPinnedIdentity(
    aggregate.items,
    limits.tasks,
    task => task.id === pinnedSelection?.taskId,
  )
  const tasks = boundedTasks.items.map(task => Object.freeze({
    task,
    runs: Object.freeze(runsByTask.get(task.id) ?? []),
  }))
  return Object.freeze({
    readCursor: projection.delivery.readCursor,
    tasks: Object.freeze(tasks),
    deliveryRuns: Object.freeze(deliveryRuns),
    runs: Object.freeze(runs),
    currentWorkRunId,
    currentCandidateRef,
    omittedTasks: boundedTasks.omitted,
    omittedRuns: Math.max(0, aggregate.runs.length - runs.length
      - (currentWorkRunId !== null && !runs.some(run => run.workRunId === currentWorkRunId) ? 1 : 0)),
  })
}
