// SPDX-License-Identifier: Apache-2.0

import type {
  DeliveryDetailProjection,
  DeliveryTaskDetailProjection,
  DeliveryTaskId,
  StageRunId,
} from './generated/contracts.js'
import type { StrongFlowProjection } from './strongflow-view-model.js'
import {
  boundedItems,
  type StrongFlowRenderLimits,
} from './strongflow-rendering.js'

/** One already-delivered Evidence row of a historical StageRun. */
export type StrongFlowHistoryEvidence = DeliveryDetailProjection['evidence'][number]

export interface StrongFlowHistoryBinding {
  readonly productSessionId: string
  readonly executionJobId: string
  readonly workerId: string | null
  readonly workerSessionId: string | null
  readonly codexThreadId: string | null
}

export interface StrongFlowHistoryRun {
  readonly stageRunId: StageRunId
  readonly deliveryTaskId: DeliveryTaskId | null
  readonly stage: string
  readonly role: string
  readonly actorType: 'codex' | 'human'
  readonly attempt: number | null
  readonly status: string
  readonly startedAt: string
  readonly finishedAt: string | null
  readonly isCurrent: boolean
  readonly producedCurrentCandidate: boolean
  readonly evidenceCount: number
  readonly evidence: readonly StrongFlowHistoryEvidence[]
  readonly candidateRefs: readonly string[]
  readonly binding: StrongFlowHistoryBinding | null
}

export interface StrongFlowHistoryTaskNode {
  readonly task: DeliveryTaskDetailProjection
  readonly runs: readonly StrongFlowHistoryRun[]
}

export interface StrongFlowHistoryTree {
  readonly readCursor: StrongFlowProjection['delivery']['readCursor']
  readonly tasks: readonly StrongFlowHistoryTaskNode[]
  readonly deliveryRuns: readonly StrongFlowHistoryRun[]
  readonly runs: readonly StrongFlowHistoryRun[]
  readonly currentStageRunId: StageRunId | null
  readonly currentCandidateRef: string | null
  readonly omittedTasks: number
  readonly omittedRuns: number
}

function historyRun(
  stage: StrongFlowProjection['delivery']['stages'][number],
  currentStageRunId: StageRunId | null,
  currentCandidateRef: string | null,
  evidence: StrongFlowProjection['evidence'],
  evidenceLimit: number,
): StrongFlowHistoryRun {
  const runEvidence = evidence.filter(item => item.stageRunId === stage.id)
  const candidateRefs = [...new Set(runEvidence.map(item => item.candidateRef))]
  const binding = stage.sessionBinding ?? null
  return Object.freeze({
    stageRunId: stage.id,
    deliveryTaskId: stage.deliveryTaskId ?? null,
    stage: stage.stage,
    role: stage.role,
    actorType: stage.actorType === 'human' ? 'human' : 'codex',
    attempt: typeof stage.attempt === 'number' ? stage.attempt : null,
    status: stage.status,
    startedAt: stage.startedAt,
    finishedAt: stage.finishedAt ?? null,
    isCurrent: stage.id === currentStageRunId,
    producedCurrentCandidate: currentCandidateRef !== null
      && candidateRefs.includes(currentCandidateRef),
    evidenceCount: runEvidence.length,
    evidence: Object.freeze([...runEvidence]),
    candidateRefs: Object.freeze(candidateRefs),
    binding: binding === null ? null : Object.freeze({
      productSessionId: binding.productSessionId,
      executionJobId: binding.executionJobId,
      workerId: binding.workerId ?? null,
      workerSessionId: binding.workerSessionId ?? null,
      codexThreadId: binding.codexThreadId ?? null,
    }),
  })
}

/**
 * Project one bounded Delivery snapshot onto the navigable history tree.
 * The association truth is the StageRun's own `deliveryTaskId`, so Task nodes,
 * the Delivery-level stage list, attempt numbering, and per-run Evidence all
 * derive from the same canonical snapshot instead of a second state machine.
 * Derive it once per snapshot and share it across every history view.
 */
export function strongFlowHistoryTree(
  projection: StrongFlowProjection,
  limits: StrongFlowRenderLimits,
): StrongFlowHistoryTree {
  const currentStageRunId = projection.stage?.id ?? null
  const currentCandidateRef = projection.currentCandidate?.candidateRef ?? null
  const boundedStages = boundedItems(projection.delivery.stages, limits.stages)
  const runs = boundedStages.items.map(stage => historyRun(
    stage,
    currentStageRunId,
    currentCandidateRef,
    projection.evidence,
    limits.evidence,
  ))
  const runsByTask = new Map<string, StrongFlowHistoryRun[]>()
  const deliveryRuns: StrongFlowHistoryRun[] = []
  for (const run of runs) {
    if (run.deliveryTaskId === null) {
      deliveryRuns.push(run)
      continue
    }
    const owned = runsByTask.get(run.deliveryTaskId) ?? []
    owned.push(run)
    runsByTask.set(run.deliveryTaskId, owned)
  }
  const boundedTasks = boundedItems(projection.delivery.tasks, limits.tasks)
  const tasks = boundedTasks.items.map(task => Object.freeze({
    task,
    runs: Object.freeze(runsByTask.get(task.id) ?? []),
  }))
  return Object.freeze({
    readCursor: projection.delivery.readCursor,
    tasks: Object.freeze(tasks),
    deliveryRuns: Object.freeze(deliveryRuns),
    runs: Object.freeze(runs),
    currentStageRunId,
    currentCandidateRef,
    omittedTasks: boundedTasks.omitted,
    omittedRuns: boundedStages.omitted,
  })
}
